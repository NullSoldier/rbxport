use rbl_export::{SourcePlaylist, SourceTrack};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Deserialize)]
struct Input {
    destination: PathBuf,
    #[serde(default)] plan_only: bool,
    #[serde(default)] owned_device_ids: Vec<u32>,
    tracks: Vec<InputTrack>,
    playlists: Vec<InputPlaylist>,
}

#[derive(Deserialize)]
struct InputTrack {
    id: String,
    #[serde(default)] source_path: PathBuf,
    #[serde(default)] planned_path: Option<String>,
    #[serde(default)] analysis_available: bool,
    #[serde(default)] existing_device_id: Option<u32>,
    title: String,
    #[serde(default)] artist: String,
    #[serde(default)] album: String,
    #[serde(default)] genre: String,
    #[serde(default)] label: String,
    #[serde(default)] key: String,
    #[serde(default)] comment: String,
    #[serde(default)] date_added: String,
    #[serde(default)] release_date: String,
    #[serde(default)] bpm_x100: u32,
    #[serde(default)] duration_sec: u16,
    #[serde(default)] bitrate: u32,
    #[serde(default)] sample_rate: u32,
    #[serde(default)] file_size: u64,
}

#[derive(Deserialize)]
struct InputPlaylist {
    id: String,
    name: String,
    #[serde(default)] existing_device_id: Option<u32>,
    #[serde(default)] parent_id: String,
    #[serde(default)] folder: bool,
    #[serde(default)] track_indices: Vec<usize>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Output {
    verified: bool,
    up_to_date: bool,
    tracks: usize,
    playlists: usize,
    one_library: bool,
    warnings: Vec<rbl_export::ExportWarning>,
    track_files: Vec<OutputTrackFile>,
    playlist_files: Vec<OutputPlaylistFile>,
    removed_files: Vec<OutputRemovedFile>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OutputRemovedFile {
    device_id: u32,
    path: String,
    analysis_dir: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OutputTrackFile {
    external_id: Option<String>,
    library_id: u64,
    device_id: u32,
    path: String,
    analysis_dir: String,
    preserved: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OutputPlaylistFile {
    external_id: Option<String>,
    device_id: u32,
    preserved: bool,
}

fn internal_id(value: &str) -> u64 {
    if let Ok(parsed) = value.parse::<u64>() { return parsed & i64::MAX as u64; }
    // TrackReady uses UUIDs. This FNV-1a identifier is stable across exports
    // and is only the numeric device-library identity; the external UUID is
    // retained independently in the bridge output.
    value.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte|
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)) & i64::MAX as u64
}

fn export(input: Input) -> Result<Output, Box<dyn std::error::Error>> {
    let destination = input.destination;
    let plan_only = input.plan_only;
    let owned_device_ids = input.owned_device_ids.into_iter().collect();
    let external_ids: std::collections::BTreeMap<u64, String> = input.tracks
        .iter().map(|track| (internal_id(&track.id), track.id.clone())).collect();
    if external_ids.len() != input.tracks.len() {
        return Err("Two TrackReady track IDs map to the same device identifier".into());
    }
    let tracks = input.tracks.into_iter().map(|track| {
        let planned_audio_path = if plan_only {
            Some(track.planned_path.unwrap_or_else(|| rbl_export::planned_audio_path(
                &track.artist, &track.album,
                &track.source_path.file_name().unwrap_or_default().to_string_lossy(),
            )))
        } else { track.planned_path };
        SourceTrack {
        id: internal_id(&track.id),
        source_path: track.source_path,
        planned_audio_path,
        planned_analysis: track.analysis_available,
        existing_device_id: track.existing_device_id,
        title: track.title,
        artist: track.artist,
        album: track.album,
        genre: track.genre,
        label: track.label,
        key: track.key,
        comment: track.comment,
        date_added: track.date_added,
        release_date: track.release_date,
        bpm_x100: track.bpm_x100,
        duration_sec: track.duration_sec,
        bitrate: track.bitrate,
        sample_rate: track.sample_rate,
        file_size: track.file_size,
        ..Default::default()
    }}).collect::<Vec<_>>();
    let playlist_ids: std::collections::BTreeMap<u64, String> = input.playlists
        .iter().map(|playlist| (internal_id(&playlist.id), playlist.id.clone())).collect();
    if playlist_ids.len() != input.playlists.len() {
        return Err("Two TrackReady playlist IDs map to the same device identifier".into());
    }
    let playlists = input.playlists.into_iter().map(|playlist| SourcePlaylist {
        id: internal_id(&playlist.id),
        device_id: playlist.existing_device_id.unwrap_or(0),
        name: playlist.name,
        parent_id: if playlist.parent_id.is_empty() { 0 } else { internal_id(&playlist.parent_id) },
        folder: playlist.folder,
        track_indices: playlist.track_indices,
        ..Default::default()
    }).collect::<Vec<_>>();
    if plan_only {
        let plan = rbl_export::database_plan::export_plan(&destination, &tracks, &playlists, &owned_device_ids)?;
        let database_dir = destination.join("PIONEER/rekordbox");
        std::fs::create_dir_all(&database_dir)?;
        std::fs::write(database_dir.join("export.pdb"), &plan.export_pdb)?;
        std::fs::write(database_dir.join("exportExt.pdb"), &plan.export_ext_pdb)?;
        std::fs::write(database_dir.join("exportLibrary.db"), &plan.export_library_db)?;
        // The planned SQLite database is a new, checkpointed generation.
        // Sidecars copied from the input snapshot must not be published with it.
        for name in ["exportLibrary.db-wal", "exportLibrary.db-shm"] {
            match std::fs::remove_file(database_dir.join(name)) {
                Ok(()) => {},
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
                Err(error) => return Err(error.into()),
            }
        }
        let check = rbl_export::verify_databases(&destination)?;
        if !check.is_ok() { return Err(format!("Planned database mismatch: {check:?}").into()); }
        return Ok(Output {
            verified: true, up_to_date: false, tracks: plan.tracks.len(), playlists: plan.playlists.len(),
            one_library: true, warnings: Vec::new(),
            track_files: plan.tracks.into_iter().map(|track| OutputTrackFile {
                external_id: external_ids.get(&track.library_id).cloned(), library_id: track.library_id,
                device_id: track.device_id, path: track.path.trim_start_matches('/').to_owned(),
                analysis_dir: track.analysis_dir.trim_start_matches('/').to_owned(), preserved: track.preserved,
            }).collect(),
            playlist_files: plan.playlists.into_iter().map(|playlist| OutputPlaylistFile {
                external_id: playlist_ids.get(&playlist.library_id).cloned(), device_id: playlist.device_id,
                preserved: playlist.device_only,
            }).collect(),
            removed_files: plan.removed.into_iter().map(|removed| OutputRemovedFile {
                device_id: removed.device_id, path: removed.path.trim_start_matches('/').to_owned(),
                analysis_dir: removed.analysis_dir.trim_start_matches('/').to_owned(),
            }).collect(),
        });
    }
    let report = rbl_export::export(&destination, &tracks, &playlists)?;
    let manifest = rbl_export::Manifest::load(&destination)
        .ok_or("rbxport did not leave a sync manifest")?;
    Ok(Output {
        verified: report.verified,
        up_to_date: report.up_to_date,
        tracks: report.tracks,
        playlists: report.playlists,
        one_library: report.one_library,
        warnings: report.warnings,
        track_files: manifest.tracks.into_iter().map(|track| OutputTrackFile {
            external_id: external_ids.get(&track.library_id).cloned(),
            library_id: track.library_id,
            device_id: track.export_id,
            path: track.audio.trim_start_matches('/').to_owned(),
            analysis_dir: track.anlz_dir.trim_start_matches('/').to_owned(),
            preserved: track.preserved == Some(true),
        }).collect(),
        playlist_files: manifest.playlists.into_iter().map(|playlist| OutputPlaylistFile {
            external_id: playlist_ids.get(&playlist.library_id).cloned(),
            device_id: playlist.export_id,
            preserved: playlist.device_only,
        }).collect(),
        removed_files: Vec::new(),
    })
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args_os().skip(1);
    let input_path = args.next().ok_or("usage: trackready-rbxport-exporter <input.json>")?;
    if args.next().is_some() { return Err("usage: trackready-rbxport-exporter <input.json>".into()); }
    let input: Input = serde_json::from_slice(&std::fs::read(input_path)?)?;
    println!("{}", serde_json::to_string(&export(input)?)?);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_fit_onelibrary_signed_integers() {
        assert!(internal_id("7c2ca0c7-e4b9-4f80-8d6c-b51f0be8c4fa") <= i64::MAX as u64);
        assert!(internal_id("18446744073709551615") <= i64::MAX as u64);
    }

    #[test]
    fn plans_database_without_reading_audio() {
        let root = tempfile::tempdir().unwrap();
        let database_dir = root.path().join("usb/PIONEER/rekordbox");
        std::fs::create_dir_all(&database_dir).unwrap();
        std::fs::write(database_dir.join("exportLibrary.db-shm"), b"stale sidecar").unwrap();
        let output = export(Input {
            owned_device_ids: vec![],
            destination: root.path().join("usb"), plan_only: true,
            tracks: vec![InputTrack {
                id: "7c2ca0c7-e4b9-4f80-8d6c-b51f0be8c4fa".into(),
                source_path: root.path().join("missing.mp3"),
                planned_path: Some("/Contents/Artist/missing.mp3".into()),
                analysis_available: true,
                existing_device_id: None,
                title: "Planned".into(), artist: "Artist".into(), album: String::new(),
                genre: String::new(), label: String::new(), key: String::new(),
                comment: String::new(), date_added: String::new(), release_date: String::new(),
                bpm_x100: 0, duration_sec: 0, bitrate: 0, sample_rate: 0, file_size: 2048,
            }],
            playlists: vec![],
        }).unwrap();
        assert!(output.verified);
        let wire = serde_json::to_value(&output).unwrap();
        assert_eq!(wire["oneLibrary"], true);
        assert!(wire["trackFiles"].is_array());
        assert_eq!(output.track_files[0].path, "Contents/Artist/missing.mp3");
        assert!(output.track_files[0].analysis_dir.starts_with("PIONEER/USBANLZ/"));
        assert_eq!(output.track_files[0].external_id.as_deref(), Some("7c2ca0c7-e4b9-4f80-8d6c-b51f0be8c4fa"));
        assert!(!root.path().join("missing.mp3").exists());
        assert!(!database_dir.join("exportLibrary.db-shm").exists());
    }

    #[test]
    fn writes_a_verified_dual_library_export() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.mp3");
        std::fs::write(&source, b"trackready test audio").unwrap();
        let output = export(Input {
            owned_device_ids: vec![],
            plan_only: false,
            destination: root.path().join("usb"),
            tracks: vec![InputTrack {
                planned_path: None,
                analysis_available: false,
                existing_device_id: None,
                id: "1".into(),
                source_path: source,
                title: "Test track".into(),
                artist: "TrackReady".into(),
                album: String::new(), genre: String::new(), label: String::new(),
                key: String::new(), comment: String::new(), date_added: String::new(),
                release_date: String::new(), bpm_x100: 0, duration_sec: 0, bitrate: 0,
                sample_rate: 0, file_size: 21,
            }],
            playlists: vec![InputPlaylist {
                existing_device_id: None,
                id: "1".into(), name: "Test playlist".into(), parent_id: String::new(), folder: false,
                track_indices: vec![0],
            }],
        }).unwrap();
        let db = root.path().join("usb/PIONEER/rekordbox");
        assert!(output.verified);
        assert!(output.one_library);
        assert_eq!(output.track_files[0].external_id.as_deref(), Some("1"));
        assert!(output.track_files[0].path.starts_with("Contents/"));
        assert!(db.join("export.pdb").is_file());
        assert!(db.join("exportExt.pdb").is_file());
        assert!(db.join("exportLibrary.db").is_file());
        assert!(rbl_export::verify(&root.path().join("usb")).unwrap().is_ok());
    }

    #[test]
    fn planned_path_matches_the_local_exporter() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.mp3");
        std::fs::write(&source, b"trackready test audio").unwrap();
        let expected = rbl_export::planned_audio_path("TrackReady", "", "source.mp3");
        let result = export(Input {
            owned_device_ids: vec![],
            plan_only: false,
            destination: root.path().join("usb"),
            tracks: vec![InputTrack {
                planned_path: None,
                analysis_available: false,
                existing_device_id: None,
                id: "1".into(), source_path: source, title: "Test".into(),
                artist: "TrackReady".into(), album: String::new(), genre: String::new(),
                label: String::new(), key: String::new(), comment: String::new(),
                date_added: String::new(), release_date: String::new(),
                bpm_x100: 0, duration_sec: 0, bitrate: 0, sample_rate: 0, file_size: 21,
            }],
            playlists: vec![],
        }).unwrap();
        assert_eq!(result.track_files[0].path, expected.trim_start_matches('/'));
    }

    #[test]
    fn keeps_uuid_identity_in_the_bridge_output() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.mp3");
        std::fs::write(&source, b"trackready uuid test").unwrap();
        let output = export(Input {
            owned_device_ids: vec![],
            plan_only: false,
            destination: root.path().join("usb"),
            tracks: vec![InputTrack {
                planned_path: None,
                analysis_available: false,
                existing_device_id: None,
                id: "7c2ca0c7-e4b9-4f80-8d6c-b51f0be8c4fa".into(), source_path: source,
                title: "UUID track".into(), artist: String::new(), album: String::new(),
                genre: String::new(), label: String::new(), key: String::new(),
                comment: String::new(), date_added: String::new(), release_date: String::new(),
                bpm_x100: 0, duration_sec: 0, bitrate: 0, sample_rate: 0, file_size: 21,
            }],
            playlists: vec![InputPlaylist {
                existing_device_id: None,
                id: "178951c7-e4b9-4f80-8d6c-b51f0be8c4fa".into(), name: "UUID playlist".into(),
                parent_id: String::new(), folder: false, track_indices: vec![0],
            }],
        }).unwrap();
        assert_eq!(output.track_files[0].external_id.as_deref(), Some("7c2ca0c7-e4b9-4f80-8d6c-b51f0be8c4fa"));
        assert_eq!(output.playlist_files[0].external_id.as_deref(), Some("178951c7-e4b9-4f80-8d6c-b51f0be8c4fa"));
    }

    #[test]
    fn retains_an_unknown_device_track_when_adding_a_trackready_track() {
        let root = tempfile::tempdir().unwrap();
        let usb = root.path().join("usb");
        let device_source = root.path().join("device-only.mp3");
        let trackready_source = root.path().join("trackready.mp3");
        std::fs::write(&device_source, b"device-only audio").unwrap();
        std::fs::write(&trackready_source, b"trackready audio").unwrap();
        rbl_export::export(
            &usb,
            &[SourceTrack {
                id: 99,
                source_path: device_source,
                title: "Device only".into(),
                file_size: 17,
                ..Default::default()
            }],
            &[SourcePlaylist {
                id: 99,
                name: "Device playlist".into(),
                track_indices: vec![0],
                ..Default::default()
            }],
        ).unwrap();
        std::fs::remove_dir_all(usb.join("PIONEER/rbxport")).unwrap();
        std::fs::remove_dir_all(usb.join("Contents")).unwrap();

        let output = export(Input {
            owned_device_ids: vec![],
            plan_only: false,
            destination: usb.clone(),
            tracks: vec![InputTrack {
                planned_path: None,
                analysis_available: false,
                existing_device_id: None,
                id: "1".into(),
                source_path: trackready_source,
                title: "TrackReady".into(), artist: String::new(), album: String::new(),
                genre: String::new(), label: String::new(), key: String::new(),
                comment: String::new(), date_added: String::new(), release_date: String::new(),
                bpm_x100: 0, duration_sec: 0, bitrate: 0, sample_rate: 0, file_size: 16,
            }],
            playlists: vec![InputPlaylist {
                existing_device_id: None,
                id: "1".into(), name: "TrackReady playlist".into(), parent_id: String::new(), folder: false,
                track_indices: vec![0],
            }],
        }).unwrap();
        assert_eq!(output.tracks, 2);
        assert!(output.one_library);
        assert!(output.track_files.iter().any(|track| track.preserved));
        assert!(output.track_files.iter().any(|track| track.external_id.as_deref() == Some("1") && !track.preserved));
        assert!(rbl_export::verify_databases(&usb).unwrap().is_ok());
    }
}
