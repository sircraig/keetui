//! Create → edit → save → reopen round-trip tests against real KDBX4 files.

use keepass::db::fields;
use keepass::{Database, DatabaseKey};

fn sample_db() -> Database {
    let mut db = Database::new();
    let mut root = db.root_mut();
    root.name = "Root".into();

    let mut work = root.add_group();
    work.name = "Work".into();
    work.add_entry().edit(|e| {
        e.set_unprotected(fields::TITLE, "GitHub");
        e.set_unprotected(fields::USERNAME, "craig");
        e.set_protected(fields::PASSWORD, "hunter2");
        e.set_unprotected(fields::URL, "https://github.com");
        e.set_protected(
            fields::OTP,
            "otpauth://totp/GitHub?secret=JBSWY3DPEHPK3PXP&period=30&digits=6",
        );
    });

    db.root_mut().add_entry().edit(|e| {
        e.set_unprotected(fields::TITLE, "Bank");
        e.set_protected(fields::PASSWORD, "s3cret!");
    });

    db
}

fn save_to_bytes(db: &Database, key: &DatabaseKey) -> Vec<u8> {
    let mut buf = Vec::new();
    db.save(&mut buf, key.clone()).expect("save failed");
    buf
}

#[test]
fn password_roundtrip_preserves_structure_and_protection() {
    let key = DatabaseKey::new().with_password("test-password");
    let db = sample_db();
    let buf = save_to_bytes(&db, &key);

    let reopened = Database::parse(&buf, key).expect("reopen failed");
    assert_eq!(reopened.num_entries(), 2);

    let root = reopened.root();
    let work = root.group_by_name("Work").expect("Work group missing");
    let gh = work.entry_by_name("GitHub").expect("GitHub entry missing");
    assert_eq!(gh.get_username(), Some("craig"));
    assert_eq!(gh.get_password(), Some("hunter2"));
    assert_eq!(gh.get_url(), Some("https://github.com"));

    // protection flags survive
    assert!(gh.fields.get(fields::PASSWORD).unwrap().is_protected());
    assert!(gh.fields.get(fields::OTP).unwrap().is_protected());
    assert!(!gh.fields.get(fields::USERNAME).unwrap().is_protected());

    // TOTP parses and produces a code
    let totp = gh.get_otp().expect("otp should parse");
    assert_eq!(totp.value_at(0).code.len(), 6);
}

#[test]
fn wrong_password_fails() {
    let key = DatabaseKey::new().with_password("right");
    let buf = save_to_bytes(&sample_db(), &key);
    assert!(Database::parse(&buf, DatabaseKey::new().with_password("wrong")).is_err());
}

#[test]
fn edit_delete_move_roundtrip() {
    let key = DatabaseKey::new().with_password("test");
    let mut db = sample_db();

    // edit with history tracking
    let gh_id = db
        .root()
        .group_by_name("Work")
        .unwrap()
        .entry_by_name("GitHub")
        .unwrap()
        .id();
    db.entry_mut(gh_id).unwrap().edit_tracking(|e| {
        e.set_unprotected(fields::USERNAME, "craig2");
        e.set_protected(fields::PASSWORD, "new-password");
    });

    // delete the Bank entry
    let bank_id = db.root().entry_by_name("Bank").unwrap().id();
    db.entry_mut(bank_id).unwrap().remove();

    // move GitHub to a new group
    let personal_id = db
        .root_mut()
        .add_group()
        .edit(|g| g.name = "Personal".into())
        .id();
    db.entry_mut(gh_id).unwrap().move_to(personal_id).unwrap();

    let buf = save_to_bytes(&db, &key);
    let reopened = Database::parse(&buf, key).expect("reopen failed");

    assert_eq!(reopened.num_entries(), 1);
    let gh = reopened.entry(gh_id).expect("entry survives by id");
    assert_eq!(gh.get_username(), Some("craig2"));
    assert_eq!(gh.get_password(), Some("new-password"));
    assert_eq!(gh.parent().name, "Personal");

    // history recorded the pre-edit state
    let hist = gh.history.as_ref().expect("history present");
    assert!(!hist.get_entries().is_empty());
    assert_eq!(hist.get_entries()[0].get_username(), Some("craig"));
}

#[test]
fn keyfile_roundtrip() {
    // a raw 32-byte keyfile
    let keyfile: Vec<u8> = (0u8..32).collect();
    let key = DatabaseKey::new()
        .with_password("pw")
        .with_keyfile(&mut keyfile.as_slice())
        .unwrap();

    let buf = save_to_bytes(&sample_db(), &key);
    assert!(Database::parse(&buf, key).is_ok());

    // password alone must not open it
    assert!(Database::parse(&buf, DatabaseKey::new().with_password("pw")).is_err());
}
