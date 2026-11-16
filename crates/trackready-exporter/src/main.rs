use rbl_export::{SourcePlaylist, SourceTrack};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Deserialize)]
struct Input {
    destination: PathBuf,
    tracks: Vec<InputTrack>,
    playlists: Vec<InputPlaylist>,
}

#[derive(Deserialize)]
struct InputTrack {
    id: String,
    source_path: PathBuf,
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
    #[serde(default)] parent_id: String,
    #[serde(default)] folder: bool,
    #[serde(default)] track_indices: Vec<usize>,
}

#[derive(Serialize)]
struct Output {
    verified: bool,
    up_to_date: bool,
    tracks: usize,
    playlists: usize,
    one_library: bool,
    warnings: Vec<rbl_export::ExportWarning>,
    track_files: Vec<OutputTrackFile>,
    playlist_files: Vec<OutputPlaylistFile>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct OutputTrackFile {
    external_id: Option<String>,
    library_id: u64,
    device_id: u32,
    path: String,
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
    if let Ok(parsed) = value.parse::<u64>() { return parsed; }
    // TrackReady uses UUIDs. This FNV-1a identifier is stable across exports
    // and is only the numeric device-library identity; the external UUID is
    // retained independently in the bridge output.
    value.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte|
        (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3))
}

fn export(input: Input) -> Result<Output, Box<dyn std::error::Error>> {
    let destination = input.destination;
    let external_ids: std::collections::BTreeMap<u64, String> = input.tracks
        .iter().map(|track| (internal_id(&track.id), track.id.clone())).collect();
    if external_ids.len() != input.tracks.len() {
        return Err("Two TrackReady track IDs map to the same device identifier".into());
    }
    let tracks = input.tracks.into_iter().map(|track| SourceTrack {
        id: internal_id(&track.id),
        source_path: track.source_path,
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
    }).collect::<Vec<_>>();
    let playlist_ids: std::collections::BTreeMap<u64, String> = input.playlists
        .iter().map(|playlist| (internal_id(&playlist.id), playlist.id.clone())).collect();
    if playlist_ids.len() != input.playlists.len() {
        return Err("Two TrackReady playlist IDs map to the same device identifier".into());
    }
    let playlists = input.playlists.into_iter().map(|playlist| SourcePlaylist {
        id: internal_id(&playlist.id),
        name: playlist.name,
        parent_id: if playlist.parent_id.is_empty() { 0 } else { internal_id(&playlist.parent_id) },
        folder: playlist.folder,
        track_indices: playlist.track_indices,
        ..Default::default()
    }).collect::<Vec<_>>();
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
            preserved: track.preserved == Some(true),
        }).collect(),
        playlist_files: manifest.playlists.into_iter().map(|playlist| OutputPlaylistFile {
            external_id: playlist_ids.get(&playlist.library_id).cloned(),
            device_id: playlist.export_id,
            preserved: playlist.device_only,
        }).collect(),
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
    fn writes_a_verified_dual_library_export() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source.mp3");
        std::fs::write(&source, b"trackready test audio").unwrap();
        let output = export(Input {
            destination: root.path().join("usb"),
            tracks: vec![InputTrack {
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
            destination: usb.clone(),
            tracks: vec![InputTrack {
                id: "1".into(),
                source_path: trackready_source,
                title: "TrackReady".into(), artist: String::new(), album: String::new(),
                genre: String::new(), label: String::new(), key: String::new(),
                comment: String::new(), date_added: String::new(), release_date: String::new(),
                bpm_x100: 0, duration_sec: 0, bitrate: 0, sample_rate: 0, file_size: 16,
            }],
            playlists: vec![InputPlaylist {
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
