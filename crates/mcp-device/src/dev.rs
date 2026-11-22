//! 设备抽象：服务只依赖这个 trait，所以护栏、轨迹、每个工具的行为都能用假设备测试，不需要真机。
//! trait 本体在 `device` crate（setup 也要用），这里重新导出，保持原来的路径。

pub use device::api::DeviceApi;
