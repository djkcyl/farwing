#![deny(unsafe_op_in_unsafe_fn)]

use crate::decoder::c_arc::CArc;
use crate::decoder::c_box::CRef;
use crate::decoder::error::Rav1dResult;
use crate::decoder::include::dav1d::common::Rav1dDataProps;
use crate::decoder::include::dav1d::data::Rav1dData;

impl From<CArc<[u8]>> for Rav1dData {
    fn from(data: CArc<[u8]>) -> Self {
        let size = data.len();
        Self {
            data: Some(data),
            m: Rav1dDataProps {
                size,
                ..Default::default()
            },
        }
    }
}

impl Rav1dData {
    pub fn create(size: usize) -> Rav1dResult<Self> {
        let data = CArc::zeroed_slice(size)?;
        Ok(data.into())
    }

    pub fn wrap(data: CRef<[u8]>) -> Rav1dResult<Self> {
        Ok(CArc::wrap(data)?.into())
    }

    pub fn wrap_user_data(&mut self, user_data: CRef<u8>) -> Rav1dResult {
        self.m.user_data = Some(CArc::wrap(user_data)?);
        Ok(())
    }
}

impl AsRef<[u8]> for Rav1dData {
    fn as_ref(&self) -> &[u8] {
        match &self.data {
            Some(data) => data.as_ref(),
            None => &[],
        }
    }
}
