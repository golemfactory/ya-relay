use anyhow::Result;
use flate2::read::GzEncoder;
use flate2::Compression;
use std::borrow::Cow;
use std::ffi::OsStr;
use std::fs::DirEntry;
use std::io::{Read, Write};
use std::path::Path;
use std::process::Command;
use std::{env, fs, io, iter, mem, path};
use tiny_keccak::Hasher;

const VERSION_ENV: &str = "YA_RELAY_SERVER_VERSION";
const BUILD_COMMIT_ENV: &str = "YA_RELAY_BUILD_COMMIT";
const BUILD_DATE_ENV: &str = "YA_RELAY_BUILD_DATE";
const BUILD_NUMBER_ENV: &str = "YA_RELAY_BUILD_NUMBER";

fn env_value(name: &str) -> Option<String> {
    env::var(name)
        .ok()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn git_output(manifest_dir: &Path, args: &[&str]) -> Option<String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(manifest_dir)
        .args(args)
        .output()
        .ok()?;

    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn short_commit(commit: String) -> String {
    commit.chars().take(9).collect()
}

fn emit_git_rerun_directives(manifest_dir: &Path) {
    let Some(git_dir) = git_output(manifest_dir, &["rev-parse", "--git-dir"]) else {
        return;
    };
    let git_dir = {
        let path = Path::new(&git_dir);
        if path.is_absolute() {
            path.to_owned()
        } else {
            manifest_dir.join(path)
        }
    };
    let head = git_dir.join("HEAD");

    println!("cargo:rerun-if-changed={}", head.display());
    println!(
        "cargo:rerun-if-changed={}",
        git_dir.join("packed-refs").display()
    );

    if let Ok(head_ref) = fs::read_to_string(&head) {
        if let Some(reference) = head_ref.trim().strip_prefix("ref: ") {
            println!(
                "cargo:rerun-if-changed={}",
                git_dir.join(reference).display()
            );
        }
    }
}

fn emit_version_metadata() {
    for name in [
        BUILD_COMMIT_ENV,
        BUILD_DATE_ENV,
        BUILD_NUMBER_ENV,
        "GITHUB_SHA",
        "GITHUB_RUN_NUMBER",
        "BUILD_NUMBER",
    ] {
        println!("cargo:rerun-if-env-changed={name}");
    }

    let manifest_dir = env::var_os("CARGO_MANIFEST_DIR")
        .map(path::PathBuf::from)
        .unwrap_or_else(|| path::PathBuf::from("."));
    emit_git_rerun_directives(&manifest_dir);

    let commit = env_value(BUILD_COMMIT_ENV)
        .or_else(|| env_value("GITHUB_SHA"))
        .or_else(|| git_output(&manifest_dir, &["rev-parse", "HEAD"]))
        .map(short_commit)
        .unwrap_or_else(|| "unknown".to_owned());
    let date = env_value(BUILD_DATE_ENV)
        .or_else(|| git_output(&manifest_dir, &["show", "-s", "--format=%cs", "HEAD"]))
        .unwrap_or_else(|| "unknown-date".to_owned());
    let build_number = env_value(BUILD_NUMBER_ENV)
        .or_else(|| env_value("GITHUB_RUN_NUMBER"))
        .or_else(|| env_value("BUILD_NUMBER"))
        .unwrap_or_else(|| "local".to_owned());
    let version = format!(
        "{} ({commit} {date} build #{build_number})",
        env::var("CARGO_PKG_VERSION").unwrap()
    );

    println!("cargo:rustc-env={VERSION_ENV}={version}");
}

macro_rules! try_iter {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(e) => return Some(Err(e.into())),
        }
    };
}

fn normalize_path_into_url(base: &Path, path: &Path) -> String {
    path.strip_prefix(base)
        .unwrap()
        .components()
        .map(|c| c.as_os_str().to_str().unwrap())
        .fold(String::new(), |mut s, it| {
            s.push('/');
            s.push_str(it);
            s
        })
}

#[test]
fn test_path() {
    assert_eq!(
        normalize_path_into_url("./ui".as_ref(), "./ui/elements/info-box.js".as_ref()),
        "/elements/info-box.js"
    );
}

fn read_dir_recursive<P: AsRef<Path>>(path: P) -> Result<impl Iterator<Item = Result<DirEntry>>> {
    let mut stack = Vec::new();
    let mut de = fs::read_dir(path)?;
    Ok(iter::from_fn(move || loop {
        match de.next() {
            None => {
                let nde = stack.pop()?;
                de = nde;
            }
            Some(Err(e)) => return Some(Err(e.into())),
            Some(Ok(e)) => {
                let ft = try_iter!(e.file_type());
                if ft.is_dir() {
                    stack.push(mem::replace(&mut de, try_iter!(fs::read_dir(e.path()))));
                } else {
                    return Some(Ok(e));
                }
            }
        }
    }))
}

fn main() -> Result<()> {
    emit_version_metadata();

    let out_dir: path::PathBuf = env::var_os("OUT_DIR").unwrap().into();
    let output = fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(out_dir.join("ui.rs"))?;
    let mut output = io::BufWriter::new(output);

    writeln!(
        &mut output,
        r#"
        pub fn scope() -> Scope {{
            Scope::new("/ui")

    "#
    )?;
    let base: &Path = "ui".as_ref();

    for file in read_dir_recursive("ui")? {
        let file = file?.path();
        println!("cargo:warning=found {file:?}");
        let bytes = fs::read(&file)?;
        let mut compressor = GzEncoder::new(io::Cursor::new(bytes), Compression::best());
        let mut buffer = Vec::new();
        compressor.read_to_end(&mut buffer)?;
        let fname = {
            let mut output = [0u8; 32];
            let mut hasher = tiny_keccak::Sha3::v224();
            hasher.update(&buffer);
            hasher.finalize(&mut output);
            format!(
                "blob-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}.gz",
                output[0],
                output[1],
                output[2],
                output[2],
                output[4],
                output[5],
                output[6],
                output[7]
            )
        };
        fs::write(out_dir.join(&fname), buffer)?;
        println!("cargo:warning=generated {:?}", out_dir.join(&fname));
        writeln!(&mut output, "// {:?}", file)?;
        let fnx: &str = file.file_name().unwrap().to_str().unwrap();
        let content_type = match file.extension().and_then(OsStr::to_str) {
            Some("html") => "text/html",
            Some("js") => "application/javascript",
            _ => "application/octet-stream",
        };
        let path: Cow<'static, str> = if fnx == "index.html" {
            "/".into()
        } else {
            normalize_path_into_url(base, &file).into()
        };

        writeln!(
            &mut output,
            r#"
        .route("{path}", web::get().to(move || {{
            let body : &[u8]= include_bytes!("{fname}");
            future::ready(
                    HttpResponse::Ok()
                        .append_header(http::header::ContentEncoding::Gzip)
                        .content_type("{content_type}")
                        .body(body),
                )
            }})
         )
        "#
        )?;
        //writeln!(&mut output, "static {}: &'static [u8] = include_bytes!(\"{}\");", fnx.replace(".", "_").replace("-", "_"), fname)?;
    }

    writeln!(
        &mut output,
        r#"
        }}
    "#
    )?;

    Ok(())
}
