use anyhow::Result;
use brotli2::read::BrotliEncoder;
use std::borrow::Cow;
use std::ffi::OsStr;
use std::fs::DirEntry;
use std::io::{Read, Write};
use std::path::Path;
use std::{env, fs, io, iter, mem, path};
use tiny_keccak::Hasher;

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
                if let Some(nde) = stack.pop() {
                    de = nde;
                } else {
                    return None;
                }
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
        let mut compressor = BrotliEncoder::new(io::Cursor::new(bytes), 9);
        let mut buffer = Vec::new();
        compressor.read_to_end(&mut buffer)?;
        let fname = {
            let mut output = [0u8; 32];
            let mut hasher = tiny_keccak::Sha3::v224();
            hasher.update(&buffer);
            hasher.finalize(&mut output);
            format!(
                "blob-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}.br",
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
                        .append_header(http::header::ContentEncoding::Brotli)
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
