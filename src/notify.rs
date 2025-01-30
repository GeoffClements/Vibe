use std::thread;

use notify_rust::Notification;
use symphonia::core::meta::MetadataRevision;

pub fn notify(metadata: MetadataRevision) {
    thread::spawn(|| {
        Notification::new()
            .summary("Firefox News")
            .body("This will almost look like a real firefox notification.")
            .icon("firefox")
            .show()
            .ok();
    });
}
