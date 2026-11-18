//! The `opencode.ai` pin in the hosts file.
//!
//! The hosts file is read and written as **latin1** so the user's own bytes
//! round-trip untouched. A pinned line is a hex-led line naming
//! `opencode-pin`; unpinning removes every line that names it, including the
//! newline, and never guesses when the file cannot be read.

/// Whether the hosts text carries a pin line (`^[0-9A-Fa-f]{2}.*opencode-pin`
/// on any line).
pub fn pin_vivo(hosts: &[u8]) -> bool {
    hosts.split(|byte| *byte == b'\n').any(|linea| {
        linea.len() >= 2
            && linea[0].is_ascii_hexdigit()
            && linea[1].is_ascii_hexdigit()
            && contiene(linea, b"opencode-pin")
    })
}

/// The hosts text without any pinned line, and whether something was removed.
///
/// The whole line goes away, `\r\n` or `\n` included: a hosts file without a
/// final newline must not glue the pin onto the user's last entry.
pub fn sin_pin(hosts: &[u8]) -> (Vec<u8>, bool) {
    let mut restante = Vec::with_capacity(hosts.len());
    let mut quitado = false;
    let mut inicio = 0;
    while inicio < hosts.len() {
        let fin = hosts[inicio..]
            .iter()
            .position(|byte| *byte == b'\n')
            .map(|posicion| inicio + posicion + 1)
            .unwrap_or(hosts.len());
        let linea = &hosts[inicio..fin];
        if contiene(linea, b"opencode-pin") {
            quitado = true;
        } else {
            restante.extend_from_slice(linea);
        }
        inicio = fin;
    }
    (restante, quitado)
}

fn contiene(linea: &[u8], aguja: &[u8]) -> bool {
    linea.windows(aguja.len()).any(|ventana| ventana == aguja)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texto(lineas: &[&str]) -> Vec<u8> {
        lineas.join("\n").into_bytes()
    }

    #[test]
    fn a_hex_led_line_naming_the_pin_is_live() {
        assert!(pin_vivo(b"127.0.0.1 localhost\n2606:4700:78::141 opencode.ai # opencode-pin\n"));
        assert!(pin_vivo(b"2606:4700:78::141 opencode.ai # opencode-pin"));
        assert!(pin_vivo(b"AA00::1 opencode.ai # opencode-pin\n"));
        // Not hex-led, or no marker: not a pin.
        assert!(!pin_vivo(b"2606:4700:78::141 opencode.ai\n"));
        assert!(!pin_vivo(b"garbage opencode.ai # opencode-pin\n"));
        assert!(!pin_vivo(b"# opencode-pin\n"));
        assert!(!pin_vivo(b""));
    }

    #[test]
    fn unpinning_removes_the_whole_line_and_nothing_else() {
        let hosts = texto(&[
            "127.0.0.1 localhost",
            "2606:4700:78::141 opencode.ai # opencode-pin",
            "10.0.0.2 internal",
        ]);
        let (limpio, quitado) = sin_pin(&hosts);
        assert!(quitado);
        assert_eq!(
            String::from_utf8(limpio).unwrap(),
            "127.0.0.1 localhost\n10.0.0.2 internal"
        );
    }

    #[test]
    fn a_pin_without_a_final_newline_is_removed_cleanly() {
        let hosts = b"127.0.0.1 localhost\n2606:4700:78::141 opencode.ai # opencode-pin";
        let (limpio, quitado) = sin_pin(hosts);
        assert!(quitado);
        assert_eq!(limpio, b"127.0.0.1 localhost\n");
    }

    #[test]
    fn crlf_lines_are_removed_with_their_carriage_return() {
        let hosts = b"127.0.0.1 localhost\r\n2606:4700:78::141 opencode.ai # opencode-pin\r\n";
        let (limpio, quitado) = sin_pin(hosts);
        assert!(quitado);
        assert_eq!(limpio, b"127.0.0.1 localhost\r\n");
    }

    #[test]
    fn an_unpinned_file_is_left_byte_for_byte() {
        let hosts = texto(&["127.0.0.1 localhost", "10.0.0.2 internal"]);
        let (limpio, quitado) = sin_pin(&hosts);
        assert!(!quitado);
        assert_eq!(limpio, hosts);
    }
}
