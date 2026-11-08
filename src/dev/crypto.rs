//! Payload-only encryption; QCOW2 mapping and refcount metadata stays plaintext.
use super::graph::{invalid, u16_at, u32_at, u64_at};
use super::*;
use openssl::{
    hash::{hash, MessageDigest},
    pkcs5::pbkdf2_hmac,
    symm::{Cipher, Crypter, Mode},
};
use zeroize::Zeroizing;

#[derive(Clone)]
pub(crate) struct Secret(pub Zeroizing<Vec<u8>>);
impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("<redacted>")
    }
}
impl Secret {
    pub fn new(bytes: &[u8]) -> Self {
        Self(Zeroizing::new(bytes.to_vec()))
    }
}

pub(crate) struct PayloadCipher {
    key: Secret,
    cipher: Cipher,
    iv: Iv,
    legacy: bool,
}
enum Iv {
    Plain,
    Plain64,
    Essiv(MessageDigest),
}
fn digest(name: &str) -> Qcow2Result<MessageDigest> {
    match name {
        "sha1" => Ok(MessageDigest::sha1()),
        "sha256" => Ok(MessageDigest::sha256()),
        "sha512" => Ok(MessageDigest::sha512()),
        "sha224" => Ok(MessageDigest::sha224()),
        "sha384" => Ok(MessageDigest::sha384()),
        "ripemd160" => Ok(MessageDigest::ripemd160()),
        _ => Err(invalid("Unsupported LUKS hash algorithm")),
    }
}
fn text(bytes: &[u8]) -> Qcow2Result<&str> {
    let end = bytes
        .iter()
        .position(|b| *b == 0)
        .ok_or_else(|| invalid("Unterminated LUKS field"))?;
    std::str::from_utf8(&bytes[..end]).map_err(|_| invalid("Invalid LUKS field"))
}
fn aes_cbc(length: usize) -> Qcow2Result<Cipher> {
    match length {
        16 => Ok(Cipher::aes_128_cbc()),
        24 => Ok(Cipher::aes_192_cbc()),
        32 => Ok(Cipher::aes_256_cbc()),
        _ => Err(invalid("Unsupported AES key size")),
    }
}
fn transform(
    cipher: Cipher,
    key: &[u8],
    iv: Option<&[u8]>,
    data: &[u8],
    encrypt: bool,
) -> Qcow2Result<Zeroizing<Vec<u8>>> {
    let mut ctx = Crypter::new(
        cipher,
        if encrypt {
            Mode::Encrypt
        } else {
            Mode::Decrypt
        },
        key,
        iv,
    )
    .map_err(|_| invalid("Cipher initialization failed"))?;
    ctx.pad(false);
    let mut out = Zeroizing::new(vec![0; data.len() + cipher.block_size()]);
    let n = ctx
        .update(data, &mut out)
        .map_err(|_| invalid("Cipher operation failed"))?;
    let tail = ctx
        .finalize(&mut out[n..])
        .map_err(|_| invalid("Cipher finalization failed"))?;
    if n + tail != data.len() {
        return Err(invalid("Cipher output length mismatch"));
    }
    out.truncate(n + tail);
    Ok(out)
}
fn derive(
    secret: &[u8],
    salt: &[u8],
    iterations: u32,
    hash: MessageDigest,
    len: usize,
) -> Qcow2Result<Secret> {
    // Bound work derived from an untrusted disk header before entering PBKDF2.
    if iterations == 0 || iterations > 10_000_000 || len > 64 {
        return Err(invalid("Unsupported LUKS PBKDF parameters"));
    }
    let mut key = Secret::new(&vec![0; len]);
    pbkdf2_hmac(secret, salt, iterations as usize, hash, &mut key.0)
        .map_err(|_| invalid("LUKS key derivation failed"))?;
    Ok(key)
}
fn diffuse(block: &mut [u8], md: MessageDigest) -> Qcow2Result<()> {
    for (i, part) in block.chunks_mut(md.size()).enumerate() {
        let mut input = Zeroizing::new((i as u32).to_be_bytes().to_vec());
        input.extend_from_slice(part);
        let value = hash(md, &input).map_err(|_| invalid("LUKS diffusion failed"))?;
        part.copy_from_slice(&value[..part.len()]);
    }
    Ok(())
}
impl PayloadCipher {
    fn new(key: Secret, cipher: &str, mode: &str, legacy: bool) -> Qcow2Result<Self> {
        if cipher != "aes" {
            return Err(invalid("Unsupported LUKS cipher (supported: AES)"));
        }
        let (mode, iv) = mode
            .split_once('-')
            .ok_or_else(|| invalid("Invalid LUKS cipher mode"))?;
        let cipher = match mode {
            "cbc" => aes_cbc(key.0.len())?,
            "xts" => match key.0.len() {
                32 => Cipher::aes_128_xts(),
                64 => Cipher::aes_256_xts(),
                _ => return Err(invalid("Unsupported AES-XTS key size")),
            },
            _ => return Err(invalid("Unsupported LUKS cipher mode")),
        };
        let iv = match iv {
            "plain" => Iv::Plain,
            "plain64" => Iv::Plain64,
            s if s.starts_with("essiv:") => Iv::Essiv(digest(&s[6..])?),
            _ => return Err(invalid("Unsupported LUKS IV generator")),
        };
        Ok(Self {
            key,
            cipher,
            iv,
            legacy,
        })
    }
    pub fn apply(&self, host: u64, guest: u64, data: &mut [u8], encrypt: bool) -> Qcow2Result<()> {
        let offset = if self.legacy { guest } else { host };
        if offset % 512 != 0 || data.len() % 512 != 0 {
            return Err(invalid("Unaligned encrypted payload"));
        }
        for (i, sector) in data.chunks_mut(512).enumerate() {
            let number = offset / 512 + i as u64;
            let mut iv = [0; 16];
            match self.iv {
                Iv::Plain => iv[..4].copy_from_slice(&(number as u32).to_le_bytes()),
                Iv::Plain64 => iv[..8].copy_from_slice(&number.to_le_bytes()),
                Iv::Essiv(md) => {
                    iv[..8].copy_from_slice(&number.to_le_bytes());
                    let salt = Zeroizing::new(
                        hash(md, &self.key.0)
                            .map_err(|_| invalid("ESSIV hash failed"))?
                            .to_vec(),
                    );
                    let cipher = match salt.len() {
                        16 => Cipher::aes_128_ecb(),
                        24 => Cipher::aes_192_ecb(),
                        32 => Cipher::aes_256_ecb(),
                        _ => return Err(invalid("Unsupported ESSIV digest size")),
                    };
                    let encrypted_iv = transform(cipher, &salt, None, &iv, true)?;
                    iv.copy_from_slice(&encrypted_iv);
                }
            }
            sector.copy_from_slice(&transform(
                self.cipher,
                &self.key.0,
                Some(&iv),
                sector,
                encrypt,
            )?);
        }
        Ok(())
    }
}

impl<T: Qcow2IoOps> Qcow2Dev<T> {
    pub(crate) async fn prepare_crypto(&self) -> Qcow2Result<()> {
        if !self.info.encrypted() || self.crypto.read().await.is_some() {
            return Ok(());
        }
        let mut credential = self.secret.lock().await;
        if self.crypto.read().await.is_some() {
            return Ok(());
        }
        let secret = credential
            .as_ref()
            .ok_or_else(|| invalid("Missing encryption secret"))?;
        let h = self.header.read().await;
        let method = h.crypt_method();
        let extension = h.extension_data(0x0537be77);
        drop(h);
        let cipher = if method == 1 {
            let mut key = Secret::new(&[0; 16]);
            let n = secret.0.len().min(16);
            key.0[..n].copy_from_slice(&secret.0[..n]);
            PayloadCipher::new(key, "aes", "cbc-plain64", true)?
        } else {
            let ext = extension.ok_or_else(|| invalid("Missing LUKS header pointer"))?;
            if ext.len() != 16 || u64_at(&ext, 8) < 592 || u64_at(&ext, 8) > 32 << 20 {
                return Err(invalid("Invalid LUKS header pointer"));
            }
            let data = self
                .read_bytes(u64_at(&ext, 0), u64_at(&ext, 8) as usize)
                .await?;
            unlock_luks(&data, &secret.0)?
        };
        *self.crypto.write().await = Some(cipher);
        *credential = None;
        Ok(())
    }

    pub(crate) async fn read_payload(
        &self,
        host: u64,
        guest: u64,
        len: usize,
    ) -> Qcow2Result<Vec<u8>> {
        let bs = 1_usize << self.info.block_size_shift;
        let base = host & !(bs as u64 - 1);
        let pad = (host - base) as usize;
        let mut bytes = crate::helpers::Qcow2IoBuf::<u8>::new((pad + len).div_ceil(bs) * bs);
        let n = if self.info.external_data() {
            self.data_file
                .as_ref()
                .ok_or_else(|| invalid("Missing external data file"))?
                .read_to(base, &mut bytes)
                .await?
        } else {
            self.call_read(base, &mut bytes).await?
        };
        let needed = if self.info.encrypted() {
            (pad + len).div_ceil(512) * 512
        } else {
            pad + len
        };
        if n < needed {
            return Err(invalid("Short payload read"));
        }
        if self.info.encrypted() {
            let crypto = self.crypto.read().await;
            crypto
                .as_ref()
                .ok_or_else(|| invalid("Encryption is not initialized"))?
                .apply(
                    base,
                    guest
                        .checked_sub(pad as u64)
                        .ok_or("Invalid encrypted offset")?,
                    &mut bytes[..needed],
                    false,
                )?;
        }
        Ok(bytes[pad..pad + len].to_vec())
    }

    pub(crate) async fn allocate_payload(&self, data: &[u8], guest: u64) -> Qcow2Result<u64> {
        if !self.info.encrypted() {
            return self.allocate_object(data).await;
        }
        let (host, _) = self
            .allocate_clusters_exact(data.len().div_ceil(self.info.cluster_size()))
            .await?;
        self.flush_refcount().await?;
        let mut encrypted = Zeroizing::new(data.to_vec());
        self.crypto
            .read()
            .await
            .as_ref()
            .ok_or_else(|| invalid("Encryption is not initialized"))?
            .apply(host, guest, &mut encrypted, true)?;
        self.write_bytes(host, &encrypted).await?;
        self.call_fsync(0, 0, 0).await?;
        Ok(host)
    }
}

fn unlock_luks(data: &[u8], password: &[u8]) -> Qcow2Result<PayloadCipher> {
    if data.len() < 592 || &data[..6] != b"LUKS\xba\xbe" || u16_at(data, 6) != 1 {
        return Err(invalid("Unsupported LUKS header/version (expected LUKS1)"));
    }
    let cipher = text(&data[8..40])?;
    let mode = text(&data[40..72])?;
    let md = digest(text(&data[72..104])?)?;
    let key_len = u32_at(data, 108) as usize;
    if !matches!(key_len, 16 | 24 | 32 | 64) {
        return Err(invalid("Unsupported LUKS key length"));
    }
    // Validate algorithms even if no active keyslot matches the credential.
    PayloadCipher::new(Secret::new(&vec![0; key_len]), cipher, mode, false)?;
    let mut ranges = Vec::new();
    for i in 0..8 {
        let p = 208 + i * 48;
        let active = u32_at(data, p);
        let start = u32_at(data, p + 40) as usize * 512;
        let stripes = u32_at(data, p + 44) as usize;
        if !matches!(active, 0x00ac71f3 | 0x0000dead) || stripes == 0 || stripes > 65536 {
            return Err(invalid("Invalid LUKS keyslot parameters"));
        }
        let len = key_len
            .checked_mul(stripes)
            .ok_or("LUKS keyslot overflow")?
            .div_ceil(512)
            * 512;
        let end = start.checked_add(len).ok_or("LUKS keyslot overflow")?;
        if start < 4096 || end > data.len() || ranges.iter().any(|&(a, b)| start < b && a < end) {
            return Err(invalid("Invalid or overlapping LUKS keyslot range"));
        }
        ranges.push((start, end));
    }
    for i in 0..8 {
        let p = 208 + i * 48;
        if u32_at(data, p) != 0x00ac71f3 {
            continue;
        }
        let (start, end) = ranges[i];
        let key = derive(
            password,
            &data[p + 8..p + 40],
            u32_at(data, p + 4),
            md,
            key_len,
        )?;
        let mut split = Zeroizing::new(data[start..end].to_vec());
        PayloadCipher::new(key, cipher, mode, false)?.apply(0, 0, &mut split, false)?;
        let stripes = u32_at(data, p + 44) as usize;
        let mut master = Secret::new(&vec![0; key_len]);
        for (n, stripe) in split[..key_len * stripes].chunks_exact(key_len).enumerate() {
            for (a, b) in master.0.iter_mut().zip(stripe) {
                *a ^= *b;
            }
            if n + 1 < stripes {
                diffuse(&mut master.0, md)?;
            }
        }
        let check = derive(&master.0, &data[132..164], u32_at(data, 164), md, 20)?;
        if openssl::memcmp::eq(&check.0, &data[112..132]) {
            return PayloadCipher::new(master, cipher, mode, false);
        }
    }
    Err(invalid(
        "Incorrect encryption secret or damaged LUKS keyslots",
    ))
}

/// Native LUKS1 creation uses AES-256-XTS, SHA-256, and 4000 AF stripes.
#[derive(Debug, Clone)]
pub struct LuksOptions {
    pub iterations: u32,
}
impl Default for LuksOptions {
    fn default() -> Self {
        Self {
            iterations: 200_000,
        }
    }
}

fn create_luks(password: &[u8], options: &LuksOptions) -> Qcow2Result<(Vec<u8>, PayloadCipher)> {
    let md = MessageDigest::sha256();
    let mut master = Secret::new(&[0; 64]);
    openssl::rand::rand_bytes(&mut master.0)
        .map_err(|_| invalid("Random key generation failed"))?;
    let stride = (64_usize * 4000).div_ceil(4096) * 4096;
    let mut header = vec![0; 4096 + 8 * stride];
    header[..6].copy_from_slice(b"LUKS\xba\xbe");
    header[6..8].copy_from_slice(&1_u16.to_be_bytes());
    header[8..11].copy_from_slice(b"aes");
    header[40..51].copy_from_slice(b"xts-plain64");
    header[72..78].copy_from_slice(b"sha256");
    let sectors = (header.len() / 512) as u32;
    header[104..108].copy_from_slice(&sectors.to_be_bytes());
    header[108..112].copy_from_slice(&64_u32.to_be_bytes());
    openssl::rand::rand_bytes(&mut header[132..164])
        .map_err(|_| invalid("Random salt generation failed"))?;
    header[164..168].copy_from_slice(&options.iterations.to_be_bytes());
    let check = derive(&master.0, &header[132..164], options.iterations, md, 20)?;
    header[112..132].copy_from_slice(&check.0);
    let mut uuid = [0; 16];
    openssl::rand::rand_bytes(&mut uuid).map_err(|_| invalid("UUID generation failed"))?;
    uuid[6] = (uuid[6] & 15) | 64;
    uuid[8] = (uuid[8] & 63) | 128;
    let id = uuid
        .iter()
        .enumerate()
        .map(|(i, byte)| {
            format!(
                "{}{byte:02x}",
                if [4, 6, 8, 10].contains(&i) { "-" } else { "" }
            )
        })
        .collect::<String>();
    header[168..204].copy_from_slice(id.as_bytes());
    for i in 0..8 {
        let p = 208 + i * 48;
        let active = if i == 0 { 0x00ac71f3_u32 } else { 0x0000dead };
        header[p..p + 4].copy_from_slice(&active.to_be_bytes());
        header[p + 40..p + 44].copy_from_slice(&(((4096 + i * stride) / 512) as u32).to_be_bytes());
        header[p + 44..p + 48].copy_from_slice(&4000_u32.to_be_bytes());
    }
    header[212..216].copy_from_slice(&options.iterations.to_be_bytes());
    openssl::rand::rand_bytes(&mut header[216..248])
        .map_err(|_| invalid("Random salt generation failed"))?;
    let slot = derive(password, &header[216..248], options.iterations, md, 64)?;
    let mut split = Zeroizing::new(vec![0; 64 * 4000]);
    openssl::rand::rand_bytes(&mut split[..64 * 3999])
        .map_err(|_| invalid("AF randomness failed"))?;
    let mut accumulator = Secret::new(&[0; 64]);
    for stripe in split[..64 * 3999].chunks_exact(64) {
        for (a, b) in accumulator.0.iter_mut().zip(stripe) {
            *a ^= *b;
        }
        diffuse(&mut accumulator.0, md)?;
    }
    for (i, b) in master.0.iter().enumerate() {
        split[64 * 3999 + i] = *b ^ accumulator.0[i];
    }
    PayloadCipher::new(slot, "aes", "xts-plain64", false)?.apply(0, 0, &mut split, true)?;
    header[4096..4096 + split.len()].copy_from_slice(&split);
    Ok((
        header,
        PayloadCipher::new(master, "aes", "xts-plain64", false)?,
    ))
}

impl<T: Qcow2IoOps> Qcow2Dev<T> {
    /// Initialize encryption on an empty v3 image before writing guest data.
    /// This does not encrypt an already populated image in place.
    pub async fn initialize_luks(
        &self,
        secret: impl AsRef<[u8]>,
        options: LuksOptions,
    ) -> Qcow2Result<()> {
        let _io = self.io_lock.write().await;
        let h = self.header.read().await;
        if h.version() != 3
            || h.crypt_method() != 0
            || h.nb_snapshots() != 0
            || self.info.external_data()
        {
            return Err(invalid("LUKS initialization requires an empty, unencrypted v3 image without snapshots or external data"));
        }
        drop(h);
        self.flush_meta_locked().await?;
        if self.graph().await?.data_mappings.0 != 0 {
            return Err(invalid("Cannot initialize encryption over allocated data"));
        }
        let (header, cipher) = create_luks(secret.as_ref(), &options)?;
        self.begin_transaction().await?;
        let result = async {
            let host = self.allocate_object(&header).await?;
            let mut ext = host.to_be_bytes().to_vec();
            ext.extend((header.len() as u64).to_be_bytes());
            let mut h = self.header.write().await;
            h.set_crypt_method(2);
            h.set_extension(0x0537be77, Some(ext));
            drop(h);
            *self.crypto.write().await = Some(cipher);
            self.info.set_encrypted(true);
            self.rebuild_refcounts().await
        }
        .await;
        if result.is_err() {
            self.needs_repair.store(true, Ordering::Release);
        }
        result
    }
}
