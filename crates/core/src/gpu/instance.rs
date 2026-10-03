//! Cross-platform helpers for interpreting Windows PDH GPU instance names.
//!
//! `\GPU Engine(*)\Utilization Percentage` instances look like:
//! `pid_1234_engtype_3D_luid_0x00000000_0x0000abcd` (order varies by driver).
//! We scan for the `luid` token and consume the two hex values that follow.
//! GPU identity on Windows is fundamentally the adapter LUID, which is what DXGI
//! exposes on the same adapter — see `gpu/pdh.rs`.

/// Raw split-out fields of a GPU Engine instance name we care about.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GpuEngineInstance {
    /// `LowPart` half of the adapter LUID from the instance name.
    pub luid_low: u64,
    /// `HighPart` half (stored unsigned for comparison against DXGI's `HighPart`).
    pub luid_high: u64,
    /// Owning PID when the instance carries one.
    pub pid: Option<u64>,
}

/// Parse a GPU Engine instance name, returning nothing if it carries no LUID.
pub fn parse_gpu_engine(name: &str) -> Option<GpuEngineInstance> {
    let tokens: Vec<&str> = name.split('_').collect();
    let mut luid: Option<(u64, u64)> = None;
    let mut pid: Option<u64> = None;

    for (i, tok) in tokens.iter().enumerate() {
        match *tok {
            "luid" => {
                let low = parse_hex(tokens.get(i + 1)?)?;
                let high = parse_hex(tokens.get(i + 2)?)?;
                luid = Some((low, high));
            }
            "pid" => {
                pid = tokens.get(i + 1).and_then(|t| t.parse().ok());
            }
            _ => {}
        }
    }
    luid.map(|(luid_low, luid_high)| GpuEngineInstance {
        luid_low,
        luid_high,
        pid,
    })
}

fn parse_hex(tok: &str) -> Option<u64> {
    u64::from_str_radix(tok.trim_start_matches("0x"), 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_task_manager_style_instance() {
        let g = parse_gpu_engine("pid_4520_engtype_3D_luid_0x00000000_0x000000bc").unwrap();
        assert_eq!(g.luid_low, 0);
        assert_eq!(g.luid_high, 0xbc);
        assert_eq!(g.pid, Some(4520));
    }

    #[test]
    fn parses_driver_variant_without_pid() {
        let g = parse_gpu_engine("engtype_Compute_luid_0x00000012_0x00000101").unwrap();
        assert_eq!(g.luid_low, 0x12);
        assert_eq!(g.luid_high, 0x101);
        assert_eq!(g.pid, None);
    }

    #[test]
    fn rejects_non_luid_instances() {
        assert!(parse_gpu_engine("adapter0").is_none());
        assert!(parse_gpu_engine("").is_none());
        assert!(parse_gpu_engine("luid_0xZZ").is_none());
    }
}
