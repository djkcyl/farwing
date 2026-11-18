//! 终端主控：从这台设备连到对端的 `CT_TERMINAL`（连接 `kind = 8`）。
//!
//! 对被控端来说主控只开了一条数据连接；这一条连接里可以同时列会话、
//! 开多个 shell，并把本机终端窗口逐个接上去。协议帧定义在
//! [`protocol`]，被控端的处理器也共用同一份。

pub(crate) mod cli;
pub(crate) mod client;
pub(crate) mod launch;
pub(crate) mod protocol;
pub(crate) mod relay;
pub(crate) mod service;
pub(crate) mod tty;
