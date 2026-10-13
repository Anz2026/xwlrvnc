//! Builds an X keyboard mapping from the compositor's xkb keymap.
//!
//! X keycode = evdev code + 8, which is what xkb uses too. So a keysym the
//! client looks up resolves to a keycode whose `keycode - 8` is the evdev code
//! XTest injects, and the compositor — running the same keymap, which we forward
//! verbatim — turns it back into the keysym the client meant.

use std::collections::HashMap;

use xkbcommon_rs::xkb_state::{KeyDirection, StateComponent};
use xkbcommon_rs::{Context, Keymap, KeymapFormat, State};

/// X keycode range we expose (evdev 0..=247 shifted by 8).
pub const MIN_KEYCODE: u8 = 8;
pub const MAX_KEYCODE: u8 = 255;

/// Keysyms per keycode: base, shift, level3 (AltGr), shift+level3.
pub const SYMS_PER: u8 = 4;

/// The X keyboard mapping derived from one compositor keymap.
pub struct KeyTable {
    /// Keysyms for keycodes `MIN_KEYCODE..=MAX_KEYCODE`, `SYMS_PER` each.
    pub syms: Vec<u32>,
    /// The `GetModifierMapping` table: 8 modifiers (Shift, Lock, Control,
    /// Mod1..Mod5) of `modmap.len() / 8` X keycodes each, 0 padding.
    pub modmap: Vec<u8>,
}

/// The X modifiers, in `GetModifierMapping` order, by their xkb real-modifier
/// names.
const X_MODIFIERS: [&str; 8] = [
    "Shift", "Lock", "Control", "Mod1", "Mod2", "Mod3", "Mod4", "Mod5",
];

/// Compiles the keymap text into the keysym and modifier tables.
pub fn build(text: &str) -> Option<KeyTable> {
    let ctx = Context::new(0).ok()?;
    let keymap = Keymap::new_from_string(ctx, text, KeymapFormat::TextV1, 0).ok()?;
    let mut state = State::new(keymap.clone());

    let mask = |name| keymap.mod_get_index(name).map_or(0u32, |i| 1u32 << i);
    let shift = mask("Shift");
    let level3 = mask("Mod5"); // AltGr
    let levels = [0, shift, level3, shift | level3];

    let mut syms = Vec::with_capacity((MAX_KEYCODE - MIN_KEYCODE + 1) as usize * SYMS_PER as usize);
    for kc in MIN_KEYCODE..=MAX_KEYCODE {
        let mut key = [0u32; SYMS_PER as usize];
        for (sym, &m) in key.iter_mut().zip(&levels) {
            state.update_mask(m, 0, 0, 0, 0, 0);
            *sym = state.key_get_one_sym(u32::from(kc)).map_or(0, |s| s.raw());
        }
        // Keys without a level3 symbol resolve to the same keysyms with Mod5
        // set, so blank those out. Core clients treat the second pair as a
        // separate group, and x11vnc's modtweak uses the last column a keysym
        // appears in, so with duplicates it thinks shifted keysyms need
        // Shift+AltGr and releases Shift around them.
        if key[2] == key[0] {
            key[2] = 0;
        }
        if key[3] == key[1] {
            key[3] = 0;
        }
        syms.extend_from_slice(&key);
    }
    state.update_mask(0, 0, 0, 0, 0, 0);
    dedup(&mut syms);

    // The modifier map comes from the keymap too, so xkb options that move
    // modifiers (swapping Caps and Ctrl, say) show up in X: press each key and
    // see which real modifiers it sets, resetting any lock or latch after.
    let x_mods = X_MODIFIERS.map(mask);
    let mut per_mod: [Vec<u8>; 8] = Default::default();
    for kc in MIN_KEYCODE..=MAX_KEYCODE {
        // xkbcommon-rs panics in update_key for a keycode with no groups
        if state.key_get_layout(u32::from(kc)).is_none() {
            continue;
        }
        state.update_key(u32::from(kc), KeyDirection::Down);
        let set = state.serialize_mods(StateComponent::MODS_EFFECTIVE);
        state.update_key(u32::from(kc), KeyDirection::Up);
        state.update_mask(0, 0, 0, 0, 0, 0);
        for (keys, &m) in per_mod.iter_mut().zip(&x_mods) {
            if m != 0 && set & m != 0 {
                keys.push(kc);
            }
        }
    }
    let per = per_mod.iter().map(Vec::len).max().unwrap_or(0).max(1);
    let mut modmap = Vec::with_capacity(8 * per);
    for keys in &per_mod {
        modmap.extend_from_slice(keys);
        modmap.resize(modmap.len() + per - keys.len(), 0);
    }
    Some(KeyTable { syms, modmap })
}

/// Resolves keysyms that need Shift on one key but not on another.
///
/// Xlib's `XKeysymToKeycode` searches the table column-first, so a keysym on
/// the base level of some key wins over the same keysym on the shifted level
/// of another. With the usual `us` keymap, `less` resolves to the extra ISO
/// key (`less greater bar brokenbar`, next to left Shift) instead of
/// Shift+comma; x11vnc's `-nomodtweak` injects that keycode with the client's
/// Shift still held and types `greater`. Nothing is behind this keymap but
/// clients turning keysyms into keycodes, so such a keysym is kept on the
/// lowest keycode only, which is the main key block a matching keyboard most
/// likely pressed. A keysym that some key gives at both Shift levels is left
/// alone: that key works whatever Shift state the client holds, and it is what
/// the column-first search finds (XF86AudioPause is Shift+Play on one key and
/// plain on another, `parenleft` is Shift+9 and the keypad paren key).
fn dedup(syms: &mut [u32]) {
    let per = SYMS_PER as usize;
    // keysym -> lowest key index, or None once seen at both Shift levels of a key
    let mut owner: HashMap<u32, Option<usize>> = HashMap::new();
    for (i, key) in syms.chunks(per).enumerate() {
        for &sym in key {
            if sym == 0 {
                continue;
            }
            let lowest = owner.entry(sym).or_insert(Some(i));
            if key[0] == sym && key[1] == sym {
                *lowest = None;
            }
        }
    }
    for (i, key) in syms.chunks_mut(per).enumerate() {
        for sym in key.iter_mut() {
            if *sym != 0 && owner[sym].is_some_and(|lowest| lowest != i) {
                *sym = 0;
            }
        }
    }
}

/// Slices the table for a `GetKeyboardMapping(first_keycode, count)` request.
pub fn mapping_slice(table: &[u32], first_keycode: u8, count: u8) -> Vec<u32> {
    let per = SYMS_PER as usize;
    let start = first_keycode.saturating_sub(MIN_KEYCODE) as usize * per;
    (0..count as usize * per)
        .map(|i| table.get(start + i).copied().unwrap_or(0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A self-contained keymap (no system xkb data) with the collisions a
    /// `us` keymap has: `less`/`greater`/`bar` on the ISO key as well as on
    /// Shift+comma/period/backslash, and XF86AudioPause as both Shift+Play
    /// and its own key.
    const KEYMAP: &str = r#"xkb_keymap {
        xkb_keycodes {
            minimum = 8; maximum = 255;
            <AC01> = 38; <LFSH> = 50; <BKSL> = 51; <AB08> = 59; <AB09> = 60;
            <LSGT> = 94; <RALT> = 108; <PLAY> = 172; <PAUS> = 209;
        };
        xkb_types {
            type "ONE_LEVEL" { modifiers = none; level_name[1] = "Any"; };
            type "TWO_LEVEL" { modifiers = Shift; map[Shift] = 2;
                level_name[1] = "Base"; level_name[2] = "Shift"; };
            type "ALPHABETIC" { modifiers = Shift+Lock; map[Shift] = 2; map[Lock] = 2;
                level_name[1] = "Base"; level_name[2] = "Caps"; };
            type "FOUR_LEVEL" { modifiers = Shift+Mod5; map[Shift] = 2; map[Mod5] = 3;
                map[Shift+Mod5] = 4; level_name[1] = "Base"; level_name[2] = "Shift";
                level_name[3] = "Alt Base"; level_name[4] = "Shift Alt"; };
        };
        xkb_compatibility {
            interpret Shift_L { action = SetMods(modifiers = Shift); };
            interpret ISO_Level3_Shift { action = SetMods(modifiers = Mod5); };
        };
        xkb_symbols {
            key <AC01> { [ s, S ] };
            key <LFSH> { [ Shift_L ] };
            key <BKSL> { [ backslash, bar ] };
            key <AB08> { [ comma, less ] };
            key <AB09> { [ period, greater ] };
            key <LSGT> { type = "FOUR_LEVEL", [ less, greater, bar, brokenbar ] };
            key <RALT> { [ ISO_Level3_Shift ] };
            key <PLAY> { [ XF86AudioPlay, XF86AudioPause ] };
            key <PAUS> { [ XF86AudioPause, XF86AudioPause ] };
            modifier_map Shift { <LFSH> };
            modifier_map Mod5 { <RALT> };
        };
    };"#;

    const COMMA: u32 = 0x2c;
    const LESS: u32 = 0x3c;
    const PERIOD: u32 = 0x2e;
    const GREATER: u32 = 0x3e;
    const BACKSLASH: u32 = 0x5c;
    const BAR: u32 = 0x7c;
    const BROKENBAR: u32 = 0xa6;
    const S_LOWER: u32 = 0x73;
    const S_UPPER: u32 = 0x53;
    const PLAY: u32 = 0x1008_ff14;
    const PAUSE: u32 = 0x1008_ff31;

    fn row(t: &KeyTable, keycode: u8) -> [u32; 4] {
        mapping_slice(&t.syms, keycode, 1).try_into().unwrap()
    }

    #[test]
    fn shift_ambiguous_keysyms_resolve_to_lowest_keycode() {
        let t = build(KEYMAP).unwrap();
        // The main block keeps its shifted symbols; the ISO key loses them.
        assert_eq!(row(&t, 59), [COMMA, LESS, 0, 0]);
        assert_eq!(row(&t, 60), [PERIOD, GREATER, 0, 0]);
        assert_eq!(row(&t, 51), [BACKSLASH, BAR, 0, 0]);
        assert_eq!(row(&t, 94), [0, 0, 0, BROKENBAR]);
        // A keysym some key gives at both Shift levels is left alone
        // everywhere, so clients keep whatever key they chose before.
        assert_eq!(row(&t, 172), [PLAY, PAUSE, 0, 0]);
        assert_eq!(row(&t, 209), [PAUSE, PAUSE, 0, 0]);
        // Level3 columns stay blank for keys without level3 symbols.
        assert_eq!(row(&t, 38), [S_LOWER, S_UPPER, 0, 0]);
        // Every keysym is still reachable somewhere.
        for sym in [COMMA, LESS, PERIOD, GREATER, BACKSLASH, BAR, BROKENBAR, PLAY, PAUSE] {
            assert!(t.syms.contains(&sym), "{sym:#x}");
        }
        // Shift is still in the modifier map.
        assert_eq!(t.modmap[0], 50);
    }
}
