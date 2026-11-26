//! The Whisper models the plugin offers: the list of the Windows plugin, as the
//! GGML files of the whisper.cpp project on Hugging Face.

/// Downloads use this fixed revision of the model repository, so the sizes and
/// SHA-256 values below, taken from its Git LFS metadata, describe the served files.
/// <https://huggingface.co/ggerganov/whisper.cpp/tree/5359861c739e955e79d9a303bcbc70fb988958b1>
const REVISION: &str = "5359861c739e955e79d9a303bcbc70fb988958b1";

/// Fast on any CPU and good enough for most dictations, as on Windows.
const RECOMMENDED: &str = "base";

pub struct Model {
    /// Same IDs as the Windows plugin.
    pub id: &'static str,
    pub name: &'static str,
    pub size_bytes: u64,
    pub sha256: &'static str,
}

impl Model {
    pub fn file(&self) -> String {
        format!("ggml-{}.bin", self.id)
    }

    /// The `.en` models know only English and skip language detection.
    pub fn english_only(&self) -> bool {
        self.id.ends_with(".en")
    }

    pub fn recommended(&self) -> bool {
        self.id == RECOMMENDED
    }

    pub fn url(&self) -> String {
        format!(
            "https://huggingface.co/ggerganov/whisper.cpp/resolve/{REVISION}/{}",
            self.file()
        )
    }
}

pub const MODELS: &[Model] = &[
    Model {
        id: "tiny",
        name: "Tiny",
        size_bytes: 77_691_713,
        sha256: "be07e048e1e599ad46341c8d2a135645097a538221678b7acdd1b1919c6e1b21",
    },
    Model {
        id: "tiny.en",
        name: "Tiny",
        size_bytes: 77_704_715,
        sha256: "921e4cf8686fdd993dcd081a5da5b6c365bfde1162e72b08d75ac75289920b1f",
    },
    Model {
        id: "base",
        name: "Base",
        size_bytes: 147_951_465,
        sha256: "60ed5bc3dd14eea856493d334349b405782ddcaf0028d4b5df4088345fba2efe",
    },
    Model {
        id: "base.en",
        name: "Base",
        size_bytes: 147_964_211,
        sha256: "a03779c86df3323075f5e796cb2ce5029f00ec8869eee3fdfb897afe36c6d002",
    },
    Model {
        id: "small",
        name: "Small",
        size_bytes: 487_601_967,
        sha256: "1be3a9b2063867b937e64e2ec7483364a79917e157fa98c5d94b5c1fffea987b",
    },
    Model {
        id: "small.en",
        name: "Small",
        size_bytes: 487_614_201,
        sha256: "c6138d6d58ecc8322097e0f987c32f1be8bb0a18532a3f88f734d1bbf9c41e5d",
    },
    Model {
        id: "medium",
        name: "Medium",
        size_bytes: 1_533_763_059,
        sha256: "6c14d5adee5f86394037b4e4e8b59f1673b6cee10e3cf0b11bbdbee79c156208",
    },
    Model {
        id: "medium.en",
        name: "Medium",
        size_bytes: 1_533_774_781,
        sha256: "cc37e93478338ec7700281a7ac30a10128929eb8f427dda2e865faa8f6da4356",
    },
    Model {
        id: "large-v3-turbo",
        name: "Large V3 Turbo",
        size_bytes: 1_624_555_275,
        sha256: "1fc70f774d38eb169993ac391eea357ef47c88757ef72ee5943879b7e8e2bc69",
    },
];

pub fn find(id: &str) -> Option<&'static Model> {
    MODELS.iter().find(|m| m.id == id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_the_windows_models_once_each() {
        let ids: Vec<&str> = MODELS.iter().map(|m| m.id).collect();
        assert_eq!(
            ids,
            [
                "tiny",
                "tiny.en",
                "base",
                "base.en",
                "small",
                "small.en",
                "medium",
                "medium.en",
                "large-v3-turbo"
            ]
        );
        let recommended: Vec<&str> = MODELS
            .iter()
            .filter(|m| m.recommended())
            .map(|m| m.id)
            .collect();
        assert_eq!(recommended, ["base"]);
        for model in MODELS {
            assert_eq!(model.sha256.len(), 64);
            assert!(model.sha256.bytes().all(|b| b.is_ascii_hexdigit()));
        }
    }

    #[test]
    fn downloads_the_pinned_file() {
        let model = find("base.en").unwrap();
        assert!(model.english_only());
        assert!(!find("large-v3-turbo").unwrap().english_only());
        assert_eq!(
            model.url(),
            "https://huggingface.co/ggerganov/whisper.cpp/resolve/5359861c739e955e79d9a303bcbc70fb988958b1/ggml-base.en.bin"
        );
    }
}
