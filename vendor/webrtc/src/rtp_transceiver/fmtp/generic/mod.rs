#[cfg(test)]
mod generic_test;

use unicase::UniCase;

use super::*;

/// fmtp_consist checks that two FMTP parameters are not inconsistent.
fn fmtp_consist(a: &HashMap<String, String>, b: &HashMap<String, String>) -> bool {
    for (k, v) in a {
        if let Some(vb) = b.get(k) {
            if UniCase::new(v) != UniCase::new(vb) {
                return false;
            }
        }
    }
    for (k, v) in b {
        if let Some(va) = a.get(k) {
            if UniCase::new(v) != UniCase::new(va) {
                return false;
            }
        }
    }
    true
}

#[derive(Debug, PartialEq)]
pub(crate) struct GenericFmtp {
    pub(crate) mime_type: String,
    pub(crate) parameters: HashMap<String, String>,
}

impl Fmtp for GenericFmtp {
    fn mime_type(&self) -> &str {
        self.mime_type.as_str()
    }

    /// Match returns true if g and b are compatible fmtp descriptions
    /// The generic implementation is used for MimeTypes that are not defined
    fn match_fmtp(&self, f: &dyn Fmtp) -> bool {
        if let Some(c) = f.as_any().downcast_ref::<GenericFmtp>() {
            if self.mime_type.to_lowercase() != c.mime_type().to_lowercase() {
                return false;
            }

            if self.mime_type.eq_ignore_ascii_case("video/AV1") {
                // Profile is not a wildcard when omitted: AV1 RTP defaults to 0.
                // Keep separately registered Main/High payload mappings distinct.
                let profile = |params: &HashMap<String, String>| {
                    params
                        .get("profile")
                        .map_or(Some(0u8), |v| v.parse::<u8>().ok())
                        .filter(|p| *p <= 2)
                };
                if profile(&self.parameters).is_none()
                    || profile(&self.parameters) != profile(&c.parameters)
                {
                    return false;
                }
            }
            fmtp_consist(&self.parameters, &c.parameters)
        } else {
            false
        }
    }

    fn parameter(&self, key: &str) -> Option<&String> {
        self.parameters.get(key)
    }

    fn equal(&self, other: &dyn Fmtp) -> bool {
        other.as_any().downcast_ref::<GenericFmtp>() == Some(self)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}
