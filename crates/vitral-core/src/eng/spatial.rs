/// An alternate key and its positive cost on the decoder's score scale.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpatialAlt {
    pub ch: u8,
    pub penalty: f64,
}

/// Up to two alternate keys for one typed key.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
pub struct TouchKeyHint([Option<SpatialAlt>; 2]);

impl TouchKeyHint {
    pub fn new(primary: u8, alternatives: [Option<SpatialAlt>; 2]) -> Self {
        let mut alternatives = alternatives.map(|alt| {
            alt.filter(|alt| {
                primary.is_ascii_lowercase()
                    && alt.ch.is_ascii_lowercase()
                    && alt.ch != primary
                    && alt.penalty > 0.0
                    && alt.penalty <= 20.0
            })
        });
        if alternatives[0].is_none_or(|first| {
            alternatives[1].is_some_and(|second| second.penalty < first.penalty)
        }) {
            alternatives.swap(0, 1);
        }
        if let [Some(first), Some(second)] = alternatives {
            if first.ch == second.ch {
                alternatives[1] = None;
            }
        }
        Self(alternatives)
    }

    /// Parse the character codes and costs passed over FFI/JNI. Zero means absent.
    pub fn from_keycodes(primary: u32, alternatives: [(u32, f32); 2]) -> Self {
        Self::new(
            u8::try_from(primary).unwrap_or_default().to_ascii_lowercase(),
            alternatives.map(|(ch, penalty)| {
                u8::try_from(ch).ok().map(|ch| SpatialAlt {
                    ch,
                    penalty: f64::from(penalty),
                })
            }),
        )
    }

    pub fn is_empty(self) -> bool {
        self.0[0].is_none()
    }

    pub(super) fn for_key(self, primary: u8) -> Self {
        Self::new(primary, self.0)
    }

    pub(super) fn alternatives(self) -> impl Iterator<Item = SpatialAlt> {
        self.0.into_iter().flatten()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn touch_hint_keeps_only_distinct_lowercase_alternatives_with_valid_costs() {
        let alt = |ch, penalty| Some(SpatialAlt { ch, penalty });
        let hint = TouchKeyHint::new(b'a', [alt(b'd', 5.0), alt(b's', 2.0)]);
        assert_eq!(hint.0, [alt(b's', 2.0), alt(b'd', 5.0)]);
        let hint = TouchKeyHint::new(b'a', [alt(b's', 5.0), alt(b's', 2.0)]);
        assert_eq!(hint.0, [alt(b's', 2.0), None]);
        for penalty in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY, 0.0, -1.0, 20.1] {
            assert!(TouchKeyHint::new(b'a', [alt(b's', penalty), None]).is_empty());
        }
        for ch in [b'A', b'1', b'a', 0xff] {
            assert!(TouchKeyHint::new(b'a', [alt(ch, 1.0), None]).is_empty());
        }
        assert_eq!(
            TouchKeyHint::from_keycodes(u32::from(b'A'), [(0, 1.0), (u32::from(b's'), 20.0)]).0,
            [alt(b's', 20.0), None],
        );
    }
}
