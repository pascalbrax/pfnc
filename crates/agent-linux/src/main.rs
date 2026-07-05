//! Thin CLI entry point — see `lib.rs` for the actual logic.

use std::net::SocketAddr;

fn main() -> anyhow::Result<()> {
    let port = parse_port_arg(std::env::args().skip(1))?;

    // `quinn::Endpoint::server` needs an active Tokio runtime to bind
    // (it looks up the current runtime handle internally), so binding
    // happens inside `run`, not before entering the runtime.
    let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build()?;
    runtime.block_on(run(port))
}

async fn run(port: u16) -> anyhow::Result<()> {
    let addr: SocketAddr = format!("0.0.0.0:{port}").parse()?;

    let (cert, key) = pfnc_agent_linux::generate_self_signed_cert()?;
    let cert_for_startup_line = cert.clone();
    let endpoint = pfnc_agent_linux::bind_server(addr, cert, key)?;

    // Three prefixed, machine-parseable lines — this is how an SSH-exec-based
    // deployment (see `pfnc-vfs-sftp`'s `deploy` module) learns the bound
    // port and the cert to pin, since `--port 0` lets the OS pick a port and
    // the cert is generated fresh every run. Explicitly flushed since stdout
    // is block-buffered (not line-buffered) when it's not a tty.
    println!("PFNC-AGENT-PID {}", std::process::id());
    println!("PFNC-AGENT-PORT {}", endpoint.local_addr()?.port());
    println!("PFNC-AGENT-CERT-HEX {}", to_hex(cert_for_startup_line.as_ref()));
    std::io::Write::flush(&mut std::io::stdout()).ok();

    pfnc_agent_linux::serve(endpoint).await;
    Ok(())
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Parses the one CLI arg this agent understands: `--port <N>` (default
/// `4433`; `0` lets the OS pick an ephemeral port).
fn parse_port_arg(mut args: impl Iterator<Item = String>) -> anyhow::Result<u16> {
    match args.next() {
        None => Ok(4433),
        Some(flag) if flag == "--port" => {
            let value = args.next().ok_or_else(|| anyhow::anyhow!("--port requires a value"))?;
            Ok(value.parse()?)
        }
        Some(other) => anyhow::bail!("unrecognized argument: {other} (expected --port <N>)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_args_defaults_to_4433() {
        assert_eq!(parse_port_arg(std::iter::empty()).unwrap(), 4433);
    }

    #[test]
    fn to_hex_round_trips_recognizable_bytes() {
        assert_eq!(to_hex(&[0x00, 0xab, 0xff]), "00abff");
    }

    #[test]
    fn port_flag_is_parsed() {
        let args = vec!["--port".to_string(), "9999".to_string()];
        assert_eq!(parse_port_arg(args.into_iter()).unwrap(), 9999);
    }

    #[test]
    fn port_zero_is_accepted() {
        let args = vec!["--port".to_string(), "0".to_string()];
        assert_eq!(parse_port_arg(args.into_iter()).unwrap(), 0);
    }

    #[test]
    fn unrecognized_flag_is_an_error() {
        let args = vec!["--bogus".to_string()];
        assert!(parse_port_arg(args.into_iter()).is_err());
    }
}
