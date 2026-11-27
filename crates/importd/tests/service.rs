//! importd with dbd and secretsd on a real broker, a folder standing in for a
//! share: browse it, import text, a picture, music and a file it doesn't import,
//! then import again and find nothing new.

use std::path::{Path, PathBuf};
use std::time::Duration;

use bus_client::{CallError, Client};
use protocol::db::Doc;
use protocol::import::{BrowseResult, ImportStatus, Kind, code};
use serde_json::{Value, json};
use tokio::net::UnixListener;

struct TempDir(PathBuf);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A broker, dbd, secretsd and importd allowing folders under `files`.
async fn start(dir: &Path, files: &Path) -> Client {
    let socket = dir.join("bus.sock");
    tokio::spawn(busd::serve(UnixListener::bind(&socket).unwrap(), Box::new(busd::AllowAll), None));
    let path = dir.join("idealos.db");
    let db_bus = Client::connect(&socket).await.unwrap();
    db_bus.claim(protocol::db::SERVICE).await.unwrap();
    tokio::spawn(dbd::serve(db_bus, db::Store::open(&path).unwrap(), dbd::Config::beside(&path)));
    let secrets_bus = Client::connect(&socket).await.unwrap();
    secrets_bus.claim(protocol::secrets::SERVICE).await.unwrap();
    tokio::spawn(secretsd::serve(secrets_bus, secretsd::Store::open_in_memory(&[7; 32]).unwrap()));
    let import_bus = Client::connect(&socket).await.unwrap();
    import_bus.claim(protocol::import::SERVICE).await.unwrap();
    let config = importd::Config { mounts: dir.join("mounts"), dir_root: Some(files.to_owned()) };
    tokio::spawn(importd::serve(import_bus, config));
    Client::connect(&socket).await.unwrap()
}

async fn call(c: &Client, method: &str, params: Value) -> Result<Value, CallError> {
    c.request(protocol::import::SERVICE, method, params).await
}

async fn finished(c: &Client) -> ImportStatus {
    for _ in 0..500 {
        let s: ImportStatus = serde_json::from_value(call(c, "status", Value::Null).await.unwrap()).unwrap();
        if !s.running {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("the import never finished");
}

async fn docs(c: &Client, doc_type: &str) -> Vec<Doc> {
    let v = c.request(protocol::db::SERVICE, "query", json!({"type": doc_type})).await.unwrap();
    serde_json::from_value(v["docs"].clone()).unwrap()
}

/// A 3 × 2 PNG.
fn png() -> Vec<u8> {
    let mut out = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, 3, 2);
        enc.set_color(png::ColorType::Rgb);
        enc.write_header().unwrap().write_image_data(&[200; 18]).unwrap();
    }
    out
}

/// A second of silent WAV at `path`, tagged (ID3v2) as track `n` of an album
/// whose cover is `cover`.
fn tagged_wav(path: &Path, title: &str, n: u32, cover: &[u8]) {
    use lofty::config::WriteOptions;
    use lofty::picture::{MimeType, Picture, PictureType};
    use lofty::prelude::*;
    use lofty::tag::{Tag, TagType};
    let samples = 8000u32;
    let mut wav = b"RIFF".to_vec();
    wav.extend((36 + samples * 2).to_le_bytes());
    wav.extend(b"WAVEfmt ");
    for v in [
        16u32.to_le_bytes().to_vec(),
        vec![1, 0, 1, 0],
        8000u32.to_le_bytes().to_vec(),
        16000u32.to_le_bytes().to_vec(),
        vec![2, 0, 16, 0],
    ] {
        wav.extend(v);
    }
    wav.extend(b"data");
    wav.extend((samples * 2).to_le_bytes());
    wav.resize(wav.len() + samples as usize * 2, 0);
    std::fs::write(path, wav).unwrap();
    let mut tag = Tag::new(TagType::Id3v2);
    tag.set_title(title.into());
    tag.set_artist("The Testers".into());
    tag.set_album("Silence".into());
    tag.set_track(n);
    tag.push_picture(
        Picture::unchecked(cover.to_vec()).pic_type(PictureType::CoverFront).mime_type(MimeType::Png).build(),
    );
    tag.save_to_path(path, WriteOptions::default()).unwrap();
}

#[tokio::test]
async fn files_come_in_once_as_documents_and_blobs() {
    let dir = TempDir(std::env::temp_dir().join(format!("importd-service-{}", std::process::id())));
    let files = dir.0.join("files");
    std::fs::create_dir_all(files.join("Notes")).unwrap();
    std::fs::write(files.join("Notes/Garden plan.md"), "# Garden\n\nBeans.").unwrap();
    std::fs::write(files.join("Notes/list.txt"), "milk").unwrap();
    std::fs::write(files.join("Notes/latin1.txt"), [0x63, 0x61, 0x66, 0xe9]).unwrap();
    std::fs::write(files.join("cat.png"), png()).unwrap();
    std::fs::write(files.join("song.mp3"), b"ID3").unwrap(); // no tags lofty can read
    std::fs::write(files.join("readme.pdf"), b"%PDF").unwrap();
    std::fs::create_dir_all(files.join("Album")).unwrap();
    tagged_wav(&files.join("Album/1.wav"), "First", 1, &png());
    tagged_wav(&files.join("Album/2.wav"), "Second", 2, &png());
    let c = start(&dir.0, &files).await;
    let source = json!({"dir": {"path": files.to_str().unwrap()}});

    let top: BrowseResult =
        serde_json::from_value(call(&c, "browse", json!({"source": source})).await.unwrap()).unwrap();
    let names: Vec<(&str, Kind)> = top.entries.iter().map(|e| (e.name.as_str(), e.kind)).collect();
    assert_eq!(
        names,
        [
            ("Album", Kind::Folder),
            ("Notes", Kind::Folder),
            ("cat.png", Kind::Image),
            ("readme.pdf", Kind::Other),
            ("song.mp3", Kind::Audio)
        ]
    );
    let notes = call(&c, "browse", json!({"source": source, "path": "Notes"})).await.unwrap();
    assert_eq!(notes["entries"].as_array().unwrap().len(), 3);
    let outside = call(&c, "browse", json!({"source": source, "path": "../.."})).await.unwrap_err();
    assert_eq!(outside.code(), "bad_request");
    let elsewhere = call(&c, "browse", json!({"source": {"dir": {"path": "/"}}})).await.unwrap_err();
    assert_eq!(elsewhere.code(), code::NOT_FOUND, "only folders under importd's root");

    let paths = ["Notes", "cat.png", "song.mp3", "readme.pdf", "Album"];
    let started = call(&c, "import", json!({"source": source, "paths": paths})).await;
    assert_eq!(started.unwrap()["total"], 8);
    let s = finished(&c).await;
    assert_eq!((s.imported, s.skipped, s.failed, s.ignored), (6, 0, 1, 1), "{s:?}");
    assert!(s.errors[0].contains("latin1.txt: not UTF-8"), "{:?}", s.errors);

    let texts = docs(&c, "TextDocument").await;
    let plan = texts.iter().find(|d| d.data["title"] == "Garden plan").expect("the Markdown file");
    assert_eq!(plan.data["format"], "markdown");
    assert_eq!(plan.data["body"], "# Garden\n\nBeans.");
    assert_eq!(plan.data["source"]["path"], "Notes/Garden plan.md");
    assert!(plan.data["source"]["from"].as_str().unwrap().starts_with("dir:"));
    let images = docs(&c, "Image").await;
    assert_eq!(images.len(), 1);
    let cat = &images[0].data;
    assert_eq!(
        (cat["width"].clone(), cat["height"].clone(), cat["type"].clone()),
        (json!(3), json!(2), json!("image/png"))
    );
    let hash = cat["blob"]["$blob"].as_str().unwrap();
    assert_eq!(cat["source"]["hash"], hash, "the blob is the file");
    let info = c.request(protocol::db::SERVICE, "blob_info", json!({"hash": hash})).await.unwrap();
    assert_eq!(info["size"], png().len());

    // Music: tags, length, its audio and its cover, the cover stored once for both.
    let tracks = docs(&c, "Track").await;
    assert_eq!(tracks.len(), 3);
    let first = &tracks.iter().find(|t| t.data["title"] == "First").expect("a tagged track").data;
    assert_eq!(
        (first["artist"].clone(), first["album"].clone(), first["trackNumber"].clone()),
        (json!("The Testers"), json!("Silence"), json!(1))
    );
    assert!((first["duration"].as_f64().unwrap() - 1.0).abs() < 0.01, "{first:?}");
    assert_eq!(first["audio"]["$blob"], first["source"]["hash"]);
    let second = &tracks.iter().find(|t| t.data["title"] == "Second").unwrap().data;
    assert_eq!(first["artwork"], second["artwork"], "one cover for the album");
    let cover = first["artwork"]["$blob"].as_str().unwrap();
    assert_eq!(cover, hash, "the cover is the same picture as cat.png, so the same blob");
    let untagged = &tracks.iter().find(|t| t.data["title"] == "song").expect("imported by its name").data;
    assert!(untagged.get("artist").is_none() && untagged.get("artwork").is_none(), "{untagged:?}");

    // Again: everything's there already.
    call(&c, "import", json!({"source": source, "paths": [""]})).await.unwrap();
    let s = finished(&c).await;
    assert_eq!((s.imported, s.skipped, s.failed, s.ignored), (0, 6, 1, 1), "{s:?}");
    assert_eq!(docs(&c, "TextDocument").await.len(), 2);

    // A share needs its password first; then it's kept, and mounting is tried.
    let share = json!({"share": {"host": "127.0.0.1", "share": "Music", "user": "test", "port": 1}});
    let e = call(&c, "browse", json!({"source": share})).await.unwrap_err();
    assert_eq!(e.code(), code::UNREACHABLE);
    assert!(format!("{e:?}").contains("log in first"), "{e:?}");
    call(&c, "login", json!({"host": "127.0.0.1", "user": "test", "password": "not,real"})).await.unwrap();
    let e = call(&c, "browse", json!({"source": share})).await.unwrap_err();
    assert_eq!(e.code(), code::UNREACHABLE);
    assert!(!format!("{e:?}").contains("log in first"), "{e:?}");
}
