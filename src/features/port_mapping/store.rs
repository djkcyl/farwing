use super::Rule;
use crate::i18n::t;
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
        ensure!(!account.is_empty(), "{}", t().features.mapping_no_account);
        let base = crate::product::local_data_dir().context(t().features.mapping_no_settings_dir)?;
        Ok(Self(
            base.join("port-mapping")
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
        ensure!(bytes.len() <= 1_048_576, "{}", t().features.mapping_rules_file_too_large);
        let rules: Vec<Rule> = serde_json::from_slice(&bytes).context(t().features.mapping_rules_file_invalid)?;
        validate(&rules)?;
        Ok(rules)
    }
    pub(crate) fn save(&self, rules: &[Rule]) -> Result<()> {
        validate(rules)?;
        std::fs::create_dir_all(self.0.parent().context(t().features.mapping_rules_path_invalid)?)?;
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
    ensure!(rules.len() <= 256, "{}", t().features.mapping_too_many_rules);
    let mut ids = std::collections::HashSet::new();
    for rule in rules {
        rule.validate()?;
        ensure!(ids.insert(rule.id), "{}", t().features.mapping_duplicate_rule_id);
    }
    Ok(())
}
