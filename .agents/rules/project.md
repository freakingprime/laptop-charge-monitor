# Project Rules (`laptop-charge-monitor`)

## Git
- Work **only on `main`** (do not create `migration` or other long-lived branches).
- After implementing changes and verifying `cargo check`, `cargo test`, and `cargo build --release` pass, automatically commit and push changes to `origin/main` (never commit `power-monitor.db` or `target/`).

## Build & Testing
- The Windows executable embeds a `requireAdministrator` manifest via `build.rs` so elevated WMI (`EsifDeviceInformation`, `MSAcpi_ThermalZoneTemperature`) and NVMe SMART IOCTLs work automatically.
- Keep unit tests pure (math, formatting, delta/outlier filtering, downsampling logic) without requiring admin elevation, live hardware, or database files so `cargo test` runs in any shell.
- Do not install third-party kernel drivers (`PawnIO`, `WinRing0`); rely on signed OS/Intel user-mode DLLs (`ze_loader.dll`, `ControlLib.dll`) and elevated Windows APIs.
