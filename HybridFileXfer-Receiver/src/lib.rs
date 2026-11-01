//! HybridFileXfer 电脑接收器核心库。
//!
//! 手机端协议、文件落盘、adb 自动连接都放在这里，GUI 和命令行版本共用。

pub mod adb;
pub mod config;
pub mod control;
pub mod discovery;
pub mod engine;
pub mod i18n;
pub mod log;
pub mod proto;
pub mod receiver;
pub mod session;
pub mod pending;
pub mod util;
pub mod webserve;