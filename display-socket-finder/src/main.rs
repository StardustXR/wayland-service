use std::{
    env::args,
    fs::File,
    io::ErrorKind,
    path::{Path, PathBuf},
};

fn main() {
    match args().skip(1).next().as_ref().map(|v| v.as_str()) {
        Some("wayland") => {
            let Some(path) = get_free_wayland_socket_path() else {
                panic!("no free wayland socket");
            };
            let full_path = args().skip(2).next().is_some_and(|v| v == "full-path");
            if full_path {
                print!("{}", path.to_str().unwrap());
            } else {
                print!("{}", path.file_name().unwrap().to_str().unwrap());
            }
        }
        Some("x11") => {
            let Some(path) = get_free_x11_socket_path() else {
                panic!("no free x11 socket");
            };
            let full_path = args().skip(2).next().is_some_and(|v| v == "full-path");
            if full_path {
                print!("{}", path.to_str().unwrap());
            } else {
                print!("{}", path.file_name().unwrap().to_str().unwrap().replace('X', ":"));
            }
        }
        _ => {
            panic!("please run with \"wayland\", \"x11\" or \"help\" as the first argument")
        }
    }
}

pub fn get_free_wayland_socket_path() -> Option<PathBuf> {
    // Use XDG runtime directory for secure, user-specific sockets
    let base_dirs = directories::BaseDirs::new()?;
    let runtime_dir = base_dirs.runtime_dir()?;

    // Iterate through conventional display numbers (matches X11 behavior)
    for display in 0..=32 {
        let socket_path = runtime_dir.join(format!("wayland-{display}"));
        let socket_lock_path = runtime_dir.join(format!("wayland-{display}.lock"));

        let Ok(lock) = File::create(&socket_lock_path) else {
            continue;
        };

        if lock.try_lock().is_err() {
            continue;
        };

        // Check for zombie sockets (file exists but nothing listening)
        if socket_path.exists() {
            match std::os::unix::net::UnixStream::connect(&socket_path) {
                Ok(_) => continue, // Active compositor found - skip
                Err(e) if e.kind() == ErrorKind::ConnectionRefused => {
                    // Stale socket - safe to remove since we hold the lock
                    let _ = std::fs::remove_file(&socket_path);
                }
                Err(_) => continue, // Transient error - conservative skip
            }
        }

        // Found viable candidate: lock held, socket cleared/available
        return Some(socket_path);
    }

    None // Exhausted all conventional display numbers
}

pub fn get_free_x11_socket_path() -> Option<PathBuf> {
    // Use XDG runtime directory for secure, user-specific sockets
    let socket_dir = Path::new("/tmp/.X11-unix");

    // Iterate through conventional display numbers (matches X11 behavior)
    for display in 0..=32 {
        let socket_path = socket_dir.join(format!("X{display}"));

        // Check for zombie sockets (file exists but nothing listening)
        if socket_path.exists() {
            match std::os::unix::net::UnixStream::connect(&socket_path) {
                Ok(_) => continue, // Active compositor found - skip
                Err(e) if e.kind() == ErrorKind::ConnectionRefused => {
                    // Stale socket - safe to remove since we hold the lock
                    let _ = std::fs::remove_file(&socket_path);
                }
                Err(_) => continue, // Transient error - conservative skip
            }
        }

        // Found viable candidate: lock held, socket cleared/available
        return Some(socket_path);
    }

    None // Exhausted all conventional display numbers
}
