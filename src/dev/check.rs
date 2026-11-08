use super::*;
use crate::error::Qcow2Result;
use crate::helpers::IntAlignment;
use crate::meta::{Table, TableEntry};
use futures_locks::RwLock as AsyncRwLock;
use std::collections::HashMap;
use std::ops::RangeInclusive;

/// Collect the cluster ranges built by `add_used_cluster_to_set` into a
/// sorted, deduplicated list
fn sorted_ranges(set: &HashMap<u64, RangeInclusive<u64>>) -> Vec<&RangeInclusive<u64>> {
    let mut res: Vec<_> = set.values().collect();
    res.sort_by_key(|range| *range.start());
    res.dedup();
    res
}

impl<T: Qcow2IoOps> Qcow2Dev<T> {
    fn add_used_cluster_to_set(ranges: &mut HashMap<u64, RangeInclusive<u64>>, num: u64) {
        let mut start = num;
        let mut end = num;

        if num > 0 {
            if let Some(range) = ranges.remove(&(num - 1)) {
                start = *range.start();
                ranges.remove(&start);
            }
        }

        if let Some(range) = ranges.remove(&(num + 1)) {
            end = *range.end();
            ranges.remove(&end);
        }

        if let Some(range) = ranges.remove(&num) {
            start = start.min(*range.start());
            end = end.max(*range.end());
        }

        ranges.insert(start, start..=end);
        ranges.insert(end, start..=end);
    }

    async fn add_table_clusters<B: Table>(
        &self,
        table: &AsyncRwLock<B>,
        ranges: &mut HashMap<u64, RangeInclusive<u64>>,
    ) {
        let t = table.read().await;

        for i in 0..t.entries() {
            let e = t.get(i);

            if e.get_value() != 0 {
                Self::add_used_cluster_to_set(ranges, e.get_value() >> self.info.cluster_bits());
            }
        }
    }

    async fn add_refcount_table_clusters(
        &self,
        ranges: &mut HashMap<u64, RangeInclusive<u64>>,
    ) -> Qcow2Result<()> {
        let info = &self.info;
        let rt_range = {
            let h = self.header.read().await;

            h.reftable_offset()
                ..(h.reftable_offset() + (h.reftable_clusters() << info.cluster_bits()) as u64)
        };

        for c in rt_range.step_by(info.cluster_size()) {
            Self::add_used_cluster_to_set(ranges, c >> self.info.cluster_bits());
        }

        Ok(())
    }
    async fn add_l1_table_clusters(
        &self,
        ranges: &mut HashMap<u64, RangeInclusive<u64>>,
    ) -> Qcow2Result<()> {
        let info = &self.info;
        let cls_size = info.cluster_size();
        let l1_range = {
            let h = self.header.read().await;
            let l1_size = self
                .l1table
                .read()
                .await
                .byte_size()
                .align_up(cls_size)
                .unwrap();

            h.l1_table_offset()..(h.l1_table_offset() + l1_size as u64)
        };

        for c in l1_range.step_by(cls_size) {
            Self::add_used_cluster_to_set(ranges, c >> self.info.cluster_bits());
        }

        Ok(())
    }

    async fn add_data_clusters(
        &self,
        ranges: &mut HashMap<u64, RangeInclusive<u64>>,
    ) -> Qcow2Result<(usize, usize)> {
        let info = &self.info;
        let end = info.virtual_size();
        let mut allocated = 0;
        let mut compressed = 0;

        for start in (0..end).step_by(info.cluster_size()) {
            let entry = self.get_l2_entry(start).await?;
            if let Some((offset, count)) = entry.allocation(info.cluster_bits() as u32) {
                allocated += 1;
                compressed += usize::from(entry.is_compressed());
                let first = offset >> info.cluster_bits();
                for cluster in first..first + count as u64 {
                    Self::add_used_cluster_to_set(ranges, cluster);
                }
            }
        }
        Ok((allocated, compressed))
    }

    /// Return Host Cluster usage, such as, allocated clusters, how many of them
    /// are compressed, ...
    pub async fn qcow2_cluster_usage<F>(&self, cls_usage: F) -> Qcow2Result<()>
    where
        F: Fn(&str, &Vec<&RangeInclusive<u64>>, Option<(usize, usize)>),
    {
        let mut set: HashMap<u64, RangeInclusive<u64>> = HashMap::new();
        self.add_refcount_table_clusters(&mut set).await?;
        cls_usage("refcount_table", &sorted_ranges(&set), None);

        let mut set: HashMap<u64, RangeInclusive<u64>> = HashMap::new();
        self.add_l1_table_clusters(&mut set).await?;
        cls_usage("l1_table", &sorted_ranges(&set), None);

        let mut set: HashMap<u64, RangeInclusive<u64>> = HashMap::new();
        self.add_table_clusters(&self.l1table, &mut set).await;
        cls_usage("l2_tables", &sorted_ranges(&set), None);

        let mut set: HashMap<u64, RangeInclusive<u64>> = HashMap::new();
        self.add_table_clusters(&self.reftable, &mut set).await;
        cls_usage("refblock_tables", &sorted_ranges(&set), None);

        let mut set: HashMap<u64, RangeInclusive<u64>> = HashMap::new();
        let stat_res = self.add_data_clusters(&mut set).await?;
        cls_usage("data", &sorted_ranges(&set), Some(stat_res));

        Ok(())
    }

    pub(crate) async fn cluster_refcount(&self, host_cluster: u64) -> Qcow2Result<u64> {
        let cls = HostCluster(host_cluster);
        let rt_entry = {
            let rt_index = cls.rt_index(&self.info);
            let reftable = self.reftable.read().await;
            reftable.get(rt_index)
        };

        if rt_entry.is_zero() {
            return Ok(0);
        }

        let rb_handle = self.get_refblock(&cls, &rt_entry).await?;
        let rb = rb_handle.value().read().await;

        Ok(rb.get(cls.rb_slice_index(&self.info)).into_plain())
    }

    /// Check Qcow2 meta integrity and cluster leak
    pub async fn check(&self) -> Qcow2Result<()> {
        let report = self.check_report().await?;
        if let Some(mismatch) = report.mismatches.first() {
            return Err(super::graph::invalid(format!(
                "Refcount mismatch/leak at host cluster {}: actual {}, expected {}{}",
                mismatch.host_cluster,
                mismatch.actual,
                mismatch.expected,
                if mismatch.actual == 0 {
                    " (non-allocated cluster)"
                } else {
                    ""
                }
            )));
        }
        if report.dirty || report.corrupt {
            return Err(super::graph::invalid("Image needs dirty/corrupt recovery"));
        }
        if !report.copied_flag_errors.is_empty() {
            return Err(super::graph::invalid("Inaccurate active copied flags"));
        }
        Ok(())
    }

    pub(crate) async fn __qcow2_prep_io(&self) -> Qcow2Result<()> {
        if self.info.external_data() && self.data_file.is_none() {
            return Err("External data file must be attached".into());
        }
        self.load_l1_table().await?;
        self.load_refcount_table().await?;

        Ok(())
    }
}
