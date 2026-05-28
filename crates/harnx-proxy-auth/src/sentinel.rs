use base64::Engine as _;
use uuid::Uuid;

pub struct Sentinels {
    pub uuid_key: String,
    pub base64_key: String,
    pub url_base64_key: String,
    pub hex_key: String,
    pub email: String,
}

impl Sentinels {
    pub fn generate() -> Self {
        let uuid = Uuid::new_v4();
        let uuid_key = uuid.hyphenated().to_string();

        Self {
            hex_key: uuid.simple().to_string(),
            base64_key: base64::engine::general_purpose::STANDARD.encode(uuid.as_bytes()),
            url_base64_key: base64::engine::general_purpose::URL_SAFE_NO_PAD
                .encode(uuid.as_bytes()),
            email: uuid_key.replacen('-', "@", 1),
            uuid_key,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Sentinels;
    use base64::Engine as _;

    #[test]
    fn generate_populates_all_fields() {
        let sentinels = Sentinels::generate();

        assert!(!sentinels.uuid_key.is_empty());
        assert!(!sentinels.base64_key.is_empty());
        assert!(!sentinels.url_base64_key.is_empty());
        assert!(!sentinels.hex_key.is_empty());
        assert!(!sentinels.email.is_empty());
    }

    #[test]
    fn generate_hex_key_has_expected_length() {
        let sentinels = Sentinels::generate();

        assert_eq!(sentinels.hex_key.len(), 32);
    }

    #[test]
    fn generate_email_contains_at_sign() {
        let sentinels = Sentinels::generate();

        assert!(sentinels.email.contains('@'));
    }

    #[test]
    fn generate_base64_key_decodes() {
        let sentinels = Sentinels::generate();

        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&sentinels.base64_key)
            .unwrap();

        assert!(!decoded.is_empty());
    }

    #[test]
    fn generate_base64_variants_round_trip_to_same_bytes() {
        let sentinels = Sentinels::generate();

        let standard = base64::engine::general_purpose::STANDARD
            .decode(&sentinels.base64_key)
            .unwrap();
        let url_safe = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(&sentinels.url_base64_key)
            .unwrap();

        assert_eq!(standard.len(), 16);
        assert_eq!(standard, url_safe);
    }
}
