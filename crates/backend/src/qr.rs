//! QR payload → module matrix.
//!
//! The backend owns QR encoding so the app needs no image/QR
//! dependencies: it receives a [`QrMatrix`] and paints squares.

/// A QR code as a square matrix of modules, row-major.
/// `true` = dark module. The matrix has **no** quiet zone; painters
/// should add a margin (the app's connect screen does).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QrMatrix {
    /// Width and height in modules.
    pub size: usize,
    /// Row-major modules, `size * size` entries.
    pub modules: Vec<bool>,
}

impl QrMatrix {
    /// Whether the module at `(x, y)` is dark. Out-of-range
    /// coordinates read as light.
    #[must_use]
    pub fn dark(&self, x: usize, y: usize) -> bool {
        if x >= self.size || y >= self.size {
            return false;
        }
        self.modules[y * self.size + x]
    }
}

/// Encodes a QR payload into a module matrix.
///
/// # Errors
/// Returns the `qrcode` crate's error text when the payload does not
/// fit a QR code (unreachable for real pairing payloads).
pub fn qr_matrix(payload: &str) -> Result<QrMatrix, String> {
    let code = qrcode::QrCode::new(payload.as_bytes()).map_err(|e| e.to_string())?;
    let size = code.width();
    let modules = code.to_colors().into_iter().map(|c| c == qrcode::Color::Dark).collect();
    Ok(QrMatrix { size, modules })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matrix_has_finder_patterns() {
        let m = qr_matrix("zaprust-test-payload").unwrap();
        assert!(m.size >= 21, "smallest QR version is 21x21, got {}", m.size);
        assert_eq!(m.modules.len(), m.size * m.size);
        // The three finder patterns start with a dark module and a
        // dark 3x3 core.
        assert!(m.dark(0, 0));
        assert!(m.dark(m.size - 1, 0));
        assert!(m.dark(0, m.size - 1));
        assert!(m.dark(3, 3), "finder core must be dark");
        assert!(!m.dark(m.size, 0), "out of range reads light");
    }

    #[test]
    fn encoding_is_deterministic() {
        assert_eq!(qr_matrix("abc").unwrap(), qr_matrix("abc").unwrap());
        assert_ne!(qr_matrix("abc").unwrap(), qr_matrix("abd").unwrap());
    }
}
