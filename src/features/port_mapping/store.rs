use super::Rule;
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};
use std::{
    io::{Read, Write},
    path::PathBuf,
};

pub(crate) struct Store(PathBuf);
impl Store {
    pub(crate) fn new(account: &str, device: &str) -> Result<Self> {
        crate::account::api::validate_device_id(device)?;
        ensure!(!account.is_empty(), "无法确定规则所属账号");
        let base = crate::platform::paths::local_app_data().context("本地设置目录不可用")?;
        Ok(Self(
            base.join("OpenUUYC")
                .join("port-mapping")
                .join(format!("{:x}", Sha256::digest(account.as_bytes())))
                .join(format!("{device}.json")),
        ))
    }
    pub(crate) fn load(&self) -> Result<Vec<Rule>> {
        let file = match std::fs::File::open(&self.0) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(vec![]),
            Err(e) => return Err(e.into()),
        };
        let mut bytes = Vec::new();
        file.take(1_048_577).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= 1_048_576, "规则文件过大");
        let rules: Vec<Rule> = serde_json::from_slice(&bytes).context("端口转发规则文件无效")?;
        validate(&rules)?;
        Ok(rules)
    }
    pub(crate) fn save(&self, rules: &[Rule]) -> Result<()> {
        validate(rules)?;
        std::fs::create_dir_all(self.0.parent().context("规则路径无效")?)?;
        let tmp = self
            .0
            .with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
        let result = (|| {
            let mut f = std::fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&tmp)?;
            f.write_all(&serde_json::to_vec_pretty(rules)?)?;
            f.sync_all()?;
            drop(f);
            std::fs::rename(&tmp, &self.0)?;
            Ok::<_, anyhow::Error>(())
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(tmp);
        }
        result
    }
}
fn validate(rules: &[Rule]) -> Result<()> {
    ensure!(rules.len() <= 256, "规则过多");
    let mut ids = std::collections::HashSet::new();
    for rule in rules {
        rule.validate()?;
        ensure!(ids.insert(rule.id), "规则编号重复");
    }
    Ok(())
}
