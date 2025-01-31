use std::{collections::HashMap, thread};

use notify_rust::Notification;
use symphonia::core::meta::{MetadataRevision, StandardTagKey};

pub fn notify(metadata: MetadataRevision) {
    thread::spawn(move || {
        let notify_tags = metadata.tags().iter().filter(|tag| tag.is_known()).fold(
            HashMap::new(),
            |mut tags, tag| {
                if let Some(key) = tag.std_key {
                    match key {
                        StandardTagKey::Artist => {
                            tags.entry("artist").or_insert_with(|| tag.value.to_owned());
                        }
                        StandardTagKey::AlbumArtist => {
                            tags.insert("artist", tag.value.to_owned());
                        }
                        StandardTagKey::Album => {
                            tags.insert("album", tag.value.to_owned());
                        }
                        StandardTagKey::TrackTitle => {
                            tags.insert("track", tag.value.to_owned());
                        }
                        StandardTagKey::Date => {
                            tags.insert("date", tag.value.to_owned());
                        }
                        _ => {}
                    }
                }
                tags
            },
        );

        let mut notification = String::new();
        if let Some(track) = notify_tags.get("track") {
            notification.push_str(format!("<b>{}</b>", track).as_str());
        }
        if let Some(artist) = notify_tags.get("artist") {
            notification.push_str(format!(" by <b>{}</b>", artist).as_str());
        }
        if let Some(album) = notify_tags.get("album") {
            notification.push_str(format!(" from <b>{}</b>", album).as_str());
        }
        if let Some(date) = notify_tags.get("date") {
            notification.push_str(format!(" (<b>{}</b>)", date).as_str());
        }

        if notification.len() > 0 {
            Notification::new()
                .summary("Now playing")
                .body(&notification)
                .icon("emblem-music-symbolic")
                .timeout(6000)
                .show()
                .ok();
        }
    });
}
