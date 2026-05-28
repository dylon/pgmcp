//! On-disk registry mapping a directed `(src_role → tgt_role)` edge to the
//! safetensors file holding that edge's trained [`OuterLink`] (ADR-009 Track-B
//! Tier-3). A latent loop loads one outer link per inter-agent edge.

use anyhow::Result;
use candle_core::{DType, Device};
use candle_nn::VarMap;
use std::path::{Path, PathBuf};

use crate::rmas::outer_link::OuterLink;

pub struct LinkRegistry {
    dir: PathBuf,
}

impl LinkRegistry {
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Safetensors path for the directed `src_role → tgt_role` link. Role names
    /// are sanitized to a safe filename.
    pub fn path_for(&self, src_role: &str, tgt_role: &str) -> PathBuf {
        self.dir.join(format!(
            "outer__{}__{}.safetensors",
            sanitize(src_role),
            sanitize(tgt_role)
        ))
    }

    pub fn exists(&self, src_role: &str, tgt_role: &str) -> bool {
        self.path_for(src_role, tgt_role).exists()
    }

    pub fn save(
        &self,
        src_role: &str,
        tgt_role: &str,
        link: &OuterLink,
        varmap: &VarMap,
    ) -> Result<()> {
        link.save(varmap, &self.path_for(src_role, tgt_role))
    }

    pub fn load(
        &self,
        src_role: &str,
        tgt_role: &str,
        src_dim: usize,
        tgt_dim: usize,
        device: &Device,
        dtype: DType,
    ) -> Result<OuterLink> {
        OuterLink::load(
            &self.path_for(src_role, tgt_role),
            src_dim,
            tgt_dim,
            device,
            dtype,
            format!("{src_role}->{tgt_role}"),
        )
    }
}

fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn path_for_sanitizes_and_directs() {
        let reg = LinkRegistry::new("/tmp/links");
        let p = reg.path_for("Reflector", "Tool-Caller");
        assert_eq!(
            p,
            PathBuf::from("/tmp/links/outer__Reflector__Tool_Caller.safetensors")
        );
        // Direction matters: (a→b) ≠ (b→a).
        assert_ne!(reg.path_for("A", "B"), reg.path_for("B", "A"));
    }

    #[test]
    fn save_then_load_round_trips_an_outer_link() {
        let dir = std::env::temp_dir().join(format!("rmas_links_{}", std::process::id()));
        let reg = LinkRegistry::new(&dir);
        let dev = Device::Cpu;
        let (link, vm) = OuterLink::new(4, 4, &dev, DType::F32, "A->B").expect("build");
        reg.save("A", "B", &link, &vm).expect("save");
        assert!(reg.exists("A", "B"));
        let loaded = reg.load("A", "B", 4, 4, &dev, DType::F32).expect("load");
        assert_eq!(loaded.src_dim(), 4);
        assert_eq!(loaded.tgt_dim(), 4);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
