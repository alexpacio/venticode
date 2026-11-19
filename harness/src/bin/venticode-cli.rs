#[cfg(target_os = "linux")]
fn main() -> anyhow::Result<()> {
    use anyhow::{Context, ensure};
    use std::{
        io::{BufRead, BufReader, Read, Write},
        os::unix::net::UnixStream,
        time::Duration,
    };

    let args: Vec<_> = std::env::args().skip(1).collect();
    if args == ["--help"] || args == ["-h"] {
        println!(
            "venticode-cli ask [--json]\nRead a prompt from stdin during an approved Venticode model execution."
        );
        return Ok(());
    }
    ensure!(
        args == ["ask"] || args == ["ask", "--json"],
        "Usage: venticode-cli ask [--json]"
    );
    let path =
        std::env::var_os("VENTICODE_MODEL_SOCKET").context("This execution has no model access")?;
    let mut prompt = String::new();
    std::io::stdin().take(32769).read_to_string(&mut prompt)?;
    ensure!(
        !prompt.trim().is_empty() && prompt.len() <= 32768,
        "Enter a UTF-8 prompt of up to 32768 bytes"
    );
    let mut socket = UnixStream::connect(path).context("Model access expired or unavailable")?;
    socket.set_write_timeout(Some(Duration::from_secs(2)))?;
    socket.set_read_timeout(Some(Duration::from_secs(55)))?;
    serde_json::to_writer(&mut socket, &serde_json::json!({"prompt":prompt}))?;
    socket.write_all(b"\n")?;
    let mut text = String::new();
    BufReader::new(socket)
        .take(1024 * 1024)
        .read_line(&mut text)?;
    ensure!(text.ends_with('\n'), "Incomplete model response");
    let mut response: serde_json::Value = serde_json::from_str(&text)?;
    if let Some(error) = response["error"].as_str() {
        anyhow::bail!("{error}");
    }
    if let Some(object) = response.as_object_mut() {
        object.remove("type");
        object.remove("id");
        object.remove("query");
    }
    if args.len() == 2 {
        println!("{response}");
    } else {
        println!(
            "{}",
            response["text"]
                .as_str()
                .context("Missing model response")?
        );
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("venticode-cli requires a managed Venticode computer");
    std::process::exit(1);
}
