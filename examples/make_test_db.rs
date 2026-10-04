//! Create a small test vault: `cargo run --example make_test_db -- /tmp/test.kdbx [password]`

use keepass::db::fields;
use keepass::{Database, DatabaseKey};

fn main() {
    let mut args = std::env::args().skip(1);
    let path = args.next().expect("usage: make_test_db <out.kdbx> [password]");
    let password = args.next().unwrap_or_else(|| "test".to_string());

    let mut db = Database::new();
    let mut root = db.root_mut();
    root.name = "Test Vault".into();

    let mut work = root.add_group();
    work.name = "Work".into();
    work.add_entry().edit(|e| {
        e.set_unprotected(fields::TITLE, "GitHub");
        e.set_unprotected(fields::USERNAME, "craig");
        e.set_protected(fields::PASSWORD, "hunter2");
        e.set_unprotected(fields::URL, "https://github.com");
        e.set_protected(
            fields::OTP,
            "otpauth://totp/GitHub:craig?secret=JBSWY3DPEHPK3PXP&period=30&digits=6",
        );
    });
    work.add_entry().edit(|e| {
        e.set_unprotected(fields::TITLE, "Stripe");
        e.set_unprotected(fields::USERNAME, "billing@example.com");
        e.set_protected(fields::PASSWORD, "tr0ub4dor&3");
        e.set_unprotected(fields::URL, "dashboard.stripe.com");
        e.set_protected("API key", "sk_test_51Hxyz");
        e.set_unprotected("Account ID", "acct_1234");
        e.tags = vec!["billing".into(), "prod".into()];
    });
    let mut servers = work.add_group();
    servers.name = "Servers".into();
    for i in 1..=30 {
        servers.add_entry().edit(|e| {
            e.set_unprotected(fields::TITLE, format!("host-{i:02}"));
            e.set_unprotected(fields::USERNAME, "root");
            e.set_protected(fields::PASSWORD, format!("server-pass-{i}"));
            e.set_unprotected(fields::URL, format!("ssh://host-{i:02}.internal"));
        });
    }
    work.add_entry().edit(|e| {
        e.set_unprotected(fields::TITLE, "AWS Console");
        e.set_unprotected(fields::USERNAME, "craig@example.com");
        e.set_protected(fields::PASSWORD, "correct-horse-battery");
        e.set_unprotected(fields::URL, "https://console.aws.amazon.com");
        e.set_unprotected(fields::NOTES, "root account\nMFA on the yubikey");
    });

    let mut root = db.root_mut();
    let mut personal = root.add_group();
    personal.name = "Personal".into();
    personal.add_entry().edit(|e| {
        e.set_unprotected(fields::TITLE, "Bank");
        e.set_unprotected(fields::USERNAME, "craig99");
        e.set_protected(fields::PASSWORD, "s3cret!pass");
    });

    let mut file = std::fs::File::create(&path).expect("cannot create output file");
    db.save(&mut file, DatabaseKey::new().with_password(&password))
        .expect("save failed");
    println!("wrote {path} (password: {password})");
}
