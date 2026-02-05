use serde::{Deserialize, Serialize};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;

const SOCKET_PATH: &str = "/tmp/ewm.sock";

/// Events sent from compositor to Emacs
#[derive(Debug, Serialize)]
#[serde(tag = "event")]
enum Event {
    #[serde(rename = "new")]
    NewSurface { id: u32, app: String },
    #[serde(rename = "close")]
    CloseSurface { id: u32 },
}

/// Commands received from Emacs
#[derive(Debug, Deserialize)]
#[serde(tag = "cmd")]
enum Command {
    #[serde(rename = "layout")]
    Layout { id: u32, x: i32, y: i32, w: u32, h: u32 },
    #[serde(rename = "focus")]
    Focus { id: u32 },
}

fn send_event(stream: &mut UnixStream, event: &Event) -> std::io::Result<()> {
    let json = serde_json::to_string(event)?;
    writeln!(stream, "{}", json)?;
    stream.flush()
}

fn handle_client(mut stream: UnixStream) -> std::io::Result<()> {
    println!("Client connected");

    // Send some fake surface events to test the connection
    send_event(&mut stream, &Event::NewSurface {
        id: 1,
        app: "foot".to_string(),
    })?;
    send_event(&mut stream, &Event::NewSurface {
        id: 2,
        app: "firefox".to_string(),
    })?;

    // Read commands from Emacs
    let reader = BufReader::new(stream.try_clone()?);
    for line in reader.lines() {
        let line = line?;
        match serde_json::from_str::<Command>(&line) {
            Ok(cmd) => println!("Received: {:?}", cmd),
            Err(e) => eprintln!("Parse error: {} for line: {}", e, line),
        }
    }

    println!("Client disconnected");
    Ok(())
}

fn main() -> std::io::Result<()> {
    // Remove stale socket
    let socket_path = Path::new(SOCKET_PATH);
    if socket_path.exists() {
        std::fs::remove_file(socket_path)?;
    }

    let listener = UnixListener::bind(socket_path)?;
    println!("Listening on {}", SOCKET_PATH);

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                if let Err(e) = handle_client(stream) {
                    eprintln!("Client error: {}", e);
                }
            }
            Err(e) => eprintln!("Accept error: {}", e),
        }
    }

    Ok(())
}
