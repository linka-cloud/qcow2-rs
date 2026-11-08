use super::*;
use crate::error::Qcow2Result;
use crate::meta::{Table, TableEntry};
use std::collections::BTreeMap;
use std::ops::RangeInclusive;

impl<T: Qcow2IoOps> Qcow2Dev<T> {
    /// Physical host-cluster usage across the active disk, snapshots, VM state,
    /// and optional metadata. Data allocation statistics count mappings across
    /// all roots; physical ranges are deduplicated. External payload is excluded.
    /// Flush metadata before calling this inspection interface.
    pub async fn qcow2_cluster_usage<F>(&self, cls_usage: F) -> Qcow2Result<()>
    where
        F: Fn(&str, &Vec<&RangeInclusive<u64>>, Option<(usize, usize)>),
    {
        let _io = self.io_lock.write().await;
        if self.need_flush_meta() {
            return Err(super::graph::invalid(
                "Flush metadata before inspecting usage",
            ));
        }
        let graph = self.graph().await?;
        let mut groups: BTreeMap<&str, Vec<RangeInclusive<u64>>> = BTreeMap::new();
        for (cluster, (role, _)) in graph.owners {
            let name = match role {
                // Historically the caller accounts for the single header cluster.
                "header" => continue,
                "L1" => "l1_table",
                "L2" => "l2_tables",
                "refcount table" => "refcount_table",
                "refcount block" => "refblock_tables",
                name => name,
            };
            let ranges = groups.entry(name).or_default();
            if let Some(last) = ranges.last_mut() {
                if *last.end() + 1 == cluster {
                    *last = *last.start()..=cluster;
                    continue;
                }
            }
            ranges.push(cluster..=cluster);
        }
        for (role, ranges) in groups {
            let ranges = ranges.iter().collect();
            let stat = (role == "data").then_some(graph.data_mappings);
            cls_usage(role, &ranges, stat);
        }
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
        if !report.inconsistent_bitmaps.is_empty() {
            return Err(super::graph::invalid(
                "Persistent bitmap contents require explicit recovery",
            ));
        }
        Ok(())
    }

    pub(crate) async fn __qcow2_prep_io(&self) -> Qcow2Result<()> {
        if self.info.external_data() && self.data_file.is_none() {
            return Err("External data file must be attached".into());
        }
        self.prepare_crypto().await?;
        self.load_l1_table().await?;
        self.load_refcount_table().await?;

        Ok(())
    }
}
