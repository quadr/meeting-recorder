//! Mono views for transcription of a single recording WAV. Temporary files
//! are streamed to disk and removed on success, failure, or cancellation.
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

pub struct PreparedTracks {
    pub mic: PathBuf,
    pub system: PathBuf,
    temporary: Option<PathBuf>,
}

impl PreparedTracks {
    pub fn open(dir: &Path, base: &str) -> Result<Self, String> {
        let combined = dir.join(format!("{base}.wav"));
        if !combined.exists() {
            return Ok(Self {
                mic: dir.join(format!("{base}.mic.wav")),
                system: dir.join(format!("{base}.system.wav")),
                temporary: None,
            });
        }
        let mut reader = hound::WavReader::open(&combined).map_err(|e| e.to_string())?;
        let spec = reader.spec();
        if !(1..=2).contains(&spec.channels)
            || spec.bits_per_sample != 16
            || spec.sample_format != hound::SampleFormat::Int
        {
            return Err("Expected a 16-bit PCM mono or two-channel recording WAV".into());
        }
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_nanos();
        let temporary = std::env::temp_dir().join(format!(
            "meetrec-tracks-{}-{stamp}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&temporary).map_err(|e| e.to_string())?;
        let result = Self {
            mic: temporary.join("mic.wav"),
            system: temporary.join("system.wav"),
            temporary: Some(temporary),
        };
        let mono = hound::WavSpec {
            channels: 1,
            ..spec
        };
        let mut mic = hound::WavWriter::create(&result.mic, mono).map_err(|e| e.to_string())?;
        let mut system = if spec.channels == 2 {
            Some(hound::WavWriter::create(&result.system, mono).map_err(|e| e.to_string())?)
        } else {
            None
        };
        for (i, sample) in reader.samples::<i16>().enumerate() {
            let sample = sample.map_err(|e| e.to_string())?;
            if i % spec.channels as usize == 0 {
                mic.write_sample(sample).map_err(|e| e.to_string())?;
            } else if let Some(system) = system.as_mut() {
                system.write_sample(sample).map_err(|e| e.to_string())?;
            }
        }
        mic.finalize().map_err(|e| e.to_string())?;
        if let Some(system) = system {
            system.finalize().map_err(|e| e.to_string())?;
        }
        Ok(result)
    }
}

impl Drop for PreparedTracks {
    fn drop(&mut self) {
        if let Some(dir) = &self.temporary {
            if let Err(e) = std::fs::remove_dir_all(dir) {
                log::warn!(
                    "Could not remove temporary transcription audio {}: {e}",
                    dir.display()
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transcription_splits_channels_and_removes_temporary_audio() {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir =
            std::env::temp_dir().join(format!("meetrec-split-test-{}-{stamp}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        let mut sink =
            meeting_recorder::storage::WavSink::create_channels(&dir, "meeting.wav", 2).unwrap();
        sink.write(&[1, 10, 2, 20, -3, -30]).unwrap();
        sink.finalize().unwrap();
        let tracks = PreparedTracks::open(&dir, "meeting").unwrap();
        let temporary = tracks.temporary.clone().unwrap();
        for (path, expected) in [
            (&tracks.mic, vec![1, 2, -3]),
            (&tracks.system, vec![10, 20, -30]),
        ] {
            let mut reader = hound::WavReader::open(path).unwrap();
            assert_eq!(reader.spec().channels, 1);
            assert_eq!(
                reader
                    .samples::<i16>()
                    .collect::<Result<Vec<_>, _>>()
                    .unwrap(),
                expected
            );
        }
        drop(tracks);
        assert!(!temporary.exists());
        assert!(dir.join("meeting.wav").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
