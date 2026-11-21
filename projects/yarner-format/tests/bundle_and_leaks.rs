//! Export/import round trips, and no plaintext anywhere on disk.

use std::io::Cursor;
use std::path::Path;

use yarner_format::bundle::{self, Bundle};
use yarner_format::{Install, KdfParams};

const SECRET_TITLE: &[u8] = b"The Lighthouse Keeper's Secret";
const SECRET_LINE: &[u8] = b"Mira whispered: the lamp was never meant for ships";
const SECRET_IMAGE: &[u8] = b"PNG-ish bytes of a stormy cliff at dusk";

fn install(dir: &Path) -> Install {
    Install::open_with(dir, KdfParams::insecure_fast()).unwrap()
}

/// Every file under `dir`, recursively.
fn files(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    for e in std::fs::read_dir(dir).unwrap() {
        let p = e.unwrap().path();
        if p.is_dir() {
            out.extend(files(&p));
        } else {
            out.push(p);
        }
    }
    out
}

fn assert_no_plaintext(dir: &Path, secrets: &[&[u8]]) {
    for f in files(dir) {
        let bytes = std::fs::read(&f).unwrap();
        for s in secrets {
            // Also catch short fragments: any 8-byte window of a secret.
            for w in s.windows(8) {
                assert!(
                    !bytes.windows(w.len()).any(|x| x == w),
                    "plaintext {:?} found in {}",
                    String::from_utf8_lossy(w),
                    f.display()
                );
            }
        }
    }
}

#[test]
fn export_import_roundtrip_without_leaks() {
    let dir = tempfile::tempdir().unwrap();
    let inst = install(dir.path());
    let story = inst.create_story("pw", SECRET_TITLE).unwrap();
    let blob = story.put_blob(SECRET_IMAGE).unwrap();
    let _unused = story.put_blob(b"pending media").unwrap();
    let (mut log, _) = story.open_graph().unwrap();
    log.append(SECRET_LINE).unwrap();
    drop(log);
    let (mut plays, _) = story.open_plays().unwrap();
    plays.append(b"a save game").unwrap();
    drop(plays);

    let export_path = dir.path().join("story.yarner");
    let f = std::fs::File::create(&export_path).unwrap();
    bundle::export(
        &inst,
        &story,
        [SECRET_LINE],
        std::slice::from_ref(&blob),
        "share-pw",
        f,
    )
    .unwrap();

    assert_no_plaintext(
        dir.path(),
        &[SECRET_TITLE, SECRET_LINE, SECRET_IMAGE, b"a save game"],
    );

    // A player opens the bundle directly.
    let bytes = std::fs::read(&export_path).unwrap();
    assert!(Bundle::open(Cursor::new(&bytes), "wrong").is_err());
    let mut b = Bundle::open(Cursor::new(&bytes), "share-pw").unwrap();
    assert_eq!(&b.meta().unwrap()[..], SECRET_TITLE);
    assert_eq!(b.blob_ids(), vec![blob.clone()]);
    assert_eq!(&b.records().unwrap()[0][..], SECRET_LINE);
    assert_eq!(&b.read_blob(&blob).unwrap()[..], SECRET_IMAGE);
    let mut r = b.open_blob(&blob).unwrap();
    assert_eq!(&r.range(4, 12).unwrap()[..], &SECRET_IMAGE[4..12]);

    // Import into another install under a new passphrase. No plays come along.
    let dir2 = tempfile::tempdir().unwrap();
    let inst2 = install(dir2.path());
    let imported = bundle::import(
        &inst2,
        std::fs::File::open(&export_path).unwrap(),
        "share-pw",
        "mine",
    )
    .unwrap();
    assert_ne!(imported.id(), story.id());
    assert_eq!(&imported.meta().unwrap()[..], SECRET_TITLE);
    assert_eq!(&imported.read_blob(&blob).unwrap()[..], SECRET_IMAGE);
    let (_, recs) = imported.open_graph().unwrap();
    assert_eq!(&recs[0][..], SECRET_LINE);
    assert!(imported.open_plays().unwrap().1.is_empty());
    assert_eq!(inst2.unlock("mine").unwrap().len(), 1);
    assert!(inst2.unlock("share-pw").unwrap().is_empty());
    assert_no_plaintext(dir2.path(), &[SECRET_TITLE, SECRET_LINE, SECRET_IMAGE]);
}
