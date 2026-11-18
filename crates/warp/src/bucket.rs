//! The `/64` bucket of an IPv6 address: the first four hextets of its expanded
//! form, lowercase, exactly as the bat's `[System.Net.IPAddress]` prints them.

/// `Some("2a09:bac5:6de7:31e1")` for `2a09:bac5:6de7:31e1::1`; `None` when the
/// text is not an eight-hextet IPv6 address.
pub fn cubo(ip: &str) -> Option<String> {
    let (left, right) = match ip.split_once("::") {
        Some((left, right)) => (segmentos(left), segmentos(right)),
        None => (segmentos(ip), Vec::new()),
    };
    if left.len() + right.len() > 8 {
        return None;
    }
    let huecos = 8usize.saturating_sub(left.len() + right.len());
    let mut valores = Vec::with_capacity(8);
    for segmento in left
        .iter()
        .copied()
        .chain(std::iter::repeat_n("0", huecos))
        .chain(right.iter().copied())
    {
        let texto = if segmento.is_empty() { "0" } else { segmento };
        valores.push(u16::from_str_radix(texto, 16).ok()?);
    }
    Some(
        valores
            .iter()
            .take(4)
            .map(|valor| format!("{valor:x}"))
            .collect::<Vec<_>>()
            .join(":"),
    )
}

fn segmentos(texto: &str) -> Vec<&str> {
    if texto.is_empty() {
        Vec::new()
    } else {
        texto.split(':').collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_addresses_keep_their_first_four_hextets() {
        assert_eq!(
            cubo("2a09:bac5:6de7:31e1:1234:5678:9abc:def0").as_deref(),
            Some("2a09:bac5:6de7:31e1")
        );
        assert_eq!(
            cubo("2606:4700:78:0:0:0:0:141").as_deref(),
            Some("2606:4700:78:0")
        );
    }

    #[test]
    fn compressed_addresses_expand_the_gap() {
        assert_eq!(
            cubo("2606:4700:78::90:0:141").as_deref(),
            Some("2606:4700:78:0")
        );
        assert_eq!(cubo("::1").as_deref(), Some("0:0:0:0"));
        assert_eq!(
            cubo("2a09:bac5:6de7:31e1::").as_deref(),
            Some("2a09:bac5:6de7:31e1")
        );
    }

    #[test]
    fn hextets_are_lowercase_and_unpadded() {
        assert_eq!(
            cubo("2A09:BAC5:006D:31E1:0:0:0:1").as_deref(),
            Some("2a09:bac5:6d:31e1")
        );
        assert_eq!(cubo("2606:4700:0078::1").as_deref(), Some("2606:4700:78:0"));
    }

    #[test]
    fn short_text_is_padded_with_zeros_like_the_bat() {
        // No `::` but fewer than eight hextets: the gap is still zeros.
        assert_eq!(cubo("1:2:3").as_deref(), Some("1:2:3:0"));
        assert_eq!(cubo("").as_deref(), Some("0:0:0:0"));
    }

    #[test]
    fn malformed_text_is_not_a_bucket() {
        assert_eq!(cubo("1:2:3:4:5:6:7:8:9"), None);
        assert_eq!(cubo("1:2:3:4:5:6:7:8::9"), None);
        assert_eq!(cubo("zzzz::1"), None);
    }
}
