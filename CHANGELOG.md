# Changelog

All notable changes to this project will be documented in this file.


## [0.1.6](https://github.com/wind-mask/aw-watcher-agent/releases/tag/v0.1.6) - 2026-08-13

### Fixed

- 修复 Rust ActivityWatch 客户端编译错误，daemon 直接使用已有 `aw-client-rust`。
- 活动时间以 Pi 扩展 heartbeat、settled 和 end 的采样时间为准，daemon 不再自行生成周期性 heartbeat。
- 保留 abandoned sweeper，用于 agent 崩溃或强制中断后归档未结束 session。
- 统一累计 usage 与实际 model usage，区分实际使用模型和 `selected_model`。
- Pi resume/reload 使用独立 `session_instance_id`，并记录手动 compact/tree 产生的 usage。


## [0.1.0](https://github.com/wind-mask/aw-watcher-agent/releases/tag/v0.1.0) - 2026-04-27

### Added

- initial aw-watcher-agent release

### Other

- 🔧 chore: add project management tooling
