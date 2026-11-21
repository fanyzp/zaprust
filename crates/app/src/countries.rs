//! Country table for the pairing form: ISO 3166 alpha-2 code,
//! display name, and ITU dial code, sorted by name. The connect
//! screen's country picker reads this table; nothing else in
//! the app dials internationally, so the table lives here and
//! not in core.
//!
//! The flag for a row is the regional-indicator pair for the
//! ISO code, rendered by the device's color emoji font (the
//! same font the app uses for every emoji, see `emoji_font`).
//! The name and dial code are always on the row too, so a
//! device without a color emoji font still gets a fully usable
//! picker, just without color flags.

/// One country: ISO code, name, dial code (digits only, no `+`).
pub(crate) struct Country {
    pub(crate) iso: &'static str,
    pub(crate) name: &'static str,
    pub(crate) dial: &'static str,
}

/// All countries, sorted by name.
pub(crate) const COUNTRIES: &[Country] = &[
    Country { iso: "AF", name: "Afghanistan", dial: "93" },
    Country { iso: "AL", name: "Albania", dial: "355" },
    Country { iso: "DZ", name: "Algeria", dial: "213" },
    Country { iso: "AS", name: "American Samoa", dial: "1" },
    Country { iso: "AD", name: "Andorra", dial: "376" },
    Country { iso: "AO", name: "Angola", dial: "244" },
    Country { iso: "AI", name: "Anguilla", dial: "1" },
    Country { iso: "AQ", name: "Antarctica", dial: "672" },
    Country { iso: "AG", name: "Antigua and Barbuda", dial: "1" },
    Country { iso: "AR", name: "Argentina", dial: "54" },
    Country { iso: "AM", name: "Armenia", dial: "374" },
    Country { iso: "AW", name: "Aruba", dial: "297" },
    Country { iso: "AU", name: "Australia", dial: "61" },
    Country { iso: "AT", name: "Austria", dial: "43" },
    Country { iso: "AZ", name: "Azerbaijan", dial: "994" },
    Country { iso: "BS", name: "Bahamas", dial: "1" },
    Country { iso: "BH", name: "Bahrain", dial: "973" },
    Country { iso: "BD", name: "Bangladesh", dial: "880" },
    Country { iso: "BB", name: "Barbados", dial: "1" },
    Country { iso: "BY", name: "Belarus", dial: "375" },
    Country { iso: "BE", name: "Belgium", dial: "32" },
    Country { iso: "BZ", name: "Belize", dial: "501" },
    Country { iso: "BJ", name: "Benin", dial: "229" },
    Country { iso: "BM", name: "Bermuda", dial: "1" },
    Country { iso: "BT", name: "Bhutan", dial: "975" },
    Country { iso: "BO", name: "Bolivia", dial: "591" },
    Country { iso: "BA", name: "Bosnia and Herzegovina", dial: "387" },
    Country { iso: "BW", name: "Botswana", dial: "267" },
    Country { iso: "BR", name: "Brazil", dial: "55" },
    Country { iso: "VG", name: "British Virgin Islands", dial: "1" },
    Country { iso: "BN", name: "Brunei", dial: "673" },
    Country { iso: "BG", name: "Bulgaria", dial: "359" },
    Country { iso: "BF", name: "Burkina Faso", dial: "226" },
    Country { iso: "BI", name: "Burundi", dial: "257" },
    Country { iso: "KH", name: "Cambodia", dial: "855" },
    Country { iso: "CM", name: "Cameroon", dial: "237" },
    Country { iso: "CA", name: "Canada", dial: "1" },
    Country { iso: "CV", name: "Cape Verde", dial: "238" },
    Country { iso: "KY", name: "Cayman Islands", dial: "1" },
    Country { iso: "CF", name: "Central African Republic", dial: "236" },
    Country { iso: "TD", name: "Chad", dial: "235" },
    Country { iso: "CL", name: "Chile", dial: "56" },
    Country { iso: "CN", name: "China", dial: "86" },
    Country { iso: "CO", name: "Colombia", dial: "57" },
    Country { iso: "KM", name: "Comoros", dial: "269" },
    Country { iso: "CG", name: "Congo", dial: "242" },
    Country { iso: "CD", name: "Congo (DRC)", dial: "243" },
    Country { iso: "CK", name: "Cook Islands", dial: "682" },
    Country { iso: "CR", name: "Costa Rica", dial: "506" },
    Country { iso: "HR", name: "Croatia", dial: "385" },
    Country { iso: "CU", name: "Cuba", dial: "53" },
    Country { iso: "CW", name: "Curacao", dial: "599" },
    Country { iso: "CY", name: "Cyprus", dial: "357" },
    Country { iso: "CZ", name: "Czechia", dial: "420" },
    Country { iso: "DK", name: "Denmark", dial: "45" },
    Country { iso: "DJ", name: "Djibouti", dial: "253" },
    Country { iso: "DM", name: "Dominica", dial: "1" },
    Country { iso: "DO", name: "Dominican Republic", dial: "1" },
    Country { iso: "EC", name: "Ecuador", dial: "593" },
    Country { iso: "EG", name: "Egypt", dial: "20" },
    Country { iso: "SV", name: "El Salvador", dial: "503" },
    Country { iso: "GQ", name: "Equatorial Guinea", dial: "240" },
    Country { iso: "ER", name: "Eritrea", dial: "291" },
    Country { iso: "EE", name: "Estonia", dial: "372" },
    Country { iso: "SZ", name: "Eswatini", dial: "268" },
    Country { iso: "ET", name: "Ethiopia", dial: "251" },
    Country { iso: "FK", name: "Falkland Islands", dial: "500" },
    Country { iso: "FO", name: "Faroe Islands", dial: "298" },
    Country { iso: "FJ", name: "Fiji", dial: "679" },
    Country { iso: "FI", name: "Finland", dial: "358" },
    Country { iso: "FR", name: "France", dial: "33" },
    Country { iso: "GF", name: "French Guiana", dial: "594" },
    Country { iso: "PF", name: "French Polynesia", dial: "689" },
    Country { iso: "GA", name: "Gabon", dial: "241" },
    Country { iso: "GM", name: "Gambia", dial: "220" },
    Country { iso: "GE", name: "Georgia", dial: "995" },
    Country { iso: "DE", name: "Germany", dial: "49" },
    Country { iso: "GH", name: "Ghana", dial: "233" },
    Country { iso: "GI", name: "Gibraltar", dial: "350" },
    Country { iso: "GR", name: "Greece", dial: "30" },
    Country { iso: "GL", name: "Greenland", dial: "299" },
    Country { iso: "GD", name: "Grenada", dial: "1" },
    Country { iso: "GP", name: "Guadeloupe", dial: "590" },
    Country { iso: "GU", name: "Guam", dial: "1" },
    Country { iso: "GT", name: "Guatemala", dial: "502" },
    Country { iso: "GG", name: "Guernsey", dial: "44" },
    Country { iso: "GN", name: "Guinea", dial: "224" },
    Country { iso: "GW", name: "Guinea-Bissau", dial: "245" },
    Country { iso: "GY", name: "Guyana", dial: "592" },
    Country { iso: "HT", name: "Haiti", dial: "509" },
    Country { iso: "HN", name: "Honduras", dial: "504" },
    Country { iso: "HK", name: "Hong Kong", dial: "852" },
    Country { iso: "HU", name: "Hungary", dial: "36" },
    Country { iso: "IS", name: "Iceland", dial: "354" },
    Country { iso: "IN", name: "India", dial: "91" },
    Country { iso: "ID", name: "Indonesia", dial: "62" },
    Country { iso: "IR", name: "Iran", dial: "98" },
    Country { iso: "IQ", name: "Iraq", dial: "964" },
    Country { iso: "IE", name: "Ireland", dial: "353" },
    Country { iso: "IM", name: "Isle of Man", dial: "44" },
    Country { iso: "IL", name: "Israel", dial: "972" },
    Country { iso: "IT", name: "Italy", dial: "39" },
    Country { iso: "CI", name: "Ivory Coast", dial: "225" },
    Country { iso: "JM", name: "Jamaica", dial: "1" },
    Country { iso: "JP", name: "Japan", dial: "81" },
    Country { iso: "JE", name: "Jersey", dial: "44" },
    Country { iso: "JO", name: "Jordan", dial: "962" },
    Country { iso: "KZ", name: "Kazakhstan", dial: "7" },
    Country { iso: "KE", name: "Kenya", dial: "254" },
    Country { iso: "KI", name: "Kiribati", dial: "686" },
    Country { iso: "XK", name: "Kosovo", dial: "383" },
    Country { iso: "KW", name: "Kuwait", dial: "965" },
    Country { iso: "KG", name: "Kyrgyzstan", dial: "996" },
    Country { iso: "LA", name: "Laos", dial: "856" },
    Country { iso: "LV", name: "Latvia", dial: "371" },
    Country { iso: "LB", name: "Lebanon", dial: "961" },
    Country { iso: "LS", name: "Lesotho", dial: "266" },
    Country { iso: "LR", name: "Liberia", dial: "231" },
    Country { iso: "LY", name: "Libya", dial: "218" },
    Country { iso: "LI", name: "Liechtenstein", dial: "423" },
    Country { iso: "LT", name: "Lithuania", dial: "370" },
    Country { iso: "LU", name: "Luxembourg", dial: "352" },
    Country { iso: "MO", name: "Macao", dial: "853" },
    Country { iso: "MG", name: "Madagascar", dial: "261" },
    Country { iso: "MW", name: "Malawi", dial: "265" },
    Country { iso: "MY", name: "Malaysia", dial: "60" },
    Country { iso: "MV", name: "Maldives", dial: "960" },
    Country { iso: "ML", name: "Mali", dial: "223" },
    Country { iso: "MT", name: "Malta", dial: "356" },
    Country { iso: "MH", name: "Marshall Islands", dial: "692" },
    Country { iso: "MQ", name: "Martinique", dial: "596" },
    Country { iso: "MR", name: "Mauritania", dial: "222" },
    Country { iso: "MU", name: "Mauritius", dial: "230" },
    Country { iso: "YT", name: "Mayotte", dial: "262" },
    Country { iso: "MX", name: "Mexico", dial: "52" },
    Country { iso: "FM", name: "Micronesia", dial: "691" },
    Country { iso: "MD", name: "Moldova", dial: "373" },
    Country { iso: "MC", name: "Monaco", dial: "377" },
    Country { iso: "MN", name: "Mongolia", dial: "976" },
    Country { iso: "ME", name: "Montenegro", dial: "382" },
    Country { iso: "MS", name: "Montserrat", dial: "1" },
    Country { iso: "MA", name: "Morocco", dial: "212" },
    Country { iso: "MZ", name: "Mozambique", dial: "258" },
    Country { iso: "MM", name: "Myanmar", dial: "95" },
    Country { iso: "NA", name: "Namibia", dial: "264" },
    Country { iso: "NR", name: "Nauru", dial: "674" },
    Country { iso: "NP", name: "Nepal", dial: "977" },
    Country { iso: "NL", name: "Netherlands", dial: "31" },
    Country { iso: "NC", name: "New Caledonia", dial: "687" },
    Country { iso: "NZ", name: "New Zealand", dial: "64" },
    Country { iso: "NI", name: "Nicaragua", dial: "505" },
    Country { iso: "NE", name: "Niger", dial: "227" },
    Country { iso: "NG", name: "Nigeria", dial: "234" },
    Country { iso: "KP", name: "North Korea", dial: "850" },
    Country { iso: "MK", name: "North Macedonia", dial: "389" },
    Country { iso: "NO", name: "Norway", dial: "47" },
    Country { iso: "OM", name: "Oman", dial: "968" },
    Country { iso: "PK", name: "Pakistan", dial: "92" },
    Country { iso: "PW", name: "Palau", dial: "680" },
    Country { iso: "PS", name: "Palestine", dial: "970" },
    Country { iso: "PA", name: "Panama", dial: "507" },
    Country { iso: "PG", name: "Papua New Guinea", dial: "675" },
    Country { iso: "PY", name: "Paraguay", dial: "595" },
    Country { iso: "PE", name: "Peru", dial: "51" },
    Country { iso: "PH", name: "Philippines", dial: "63" },
    Country { iso: "PL", name: "Poland", dial: "48" },
    Country { iso: "PT", name: "Portugal", dial: "351" },
    Country { iso: "PR", name: "Puerto Rico", dial: "1" },
    Country { iso: "QA", name: "Qatar", dial: "974" },
    Country { iso: "RE", name: "Reunion", dial: "262" },
    Country { iso: "RO", name: "Romania", dial: "40" },
    Country { iso: "RU", name: "Russia", dial: "7" },
    Country { iso: "RW", name: "Rwanda", dial: "250" },
    Country { iso: "SH", name: "Saint Helena", dial: "290" },
    Country { iso: "KN", name: "Saint Kitts and Nevis", dial: "1" },
    Country { iso: "LC", name: "Saint Lucia", dial: "1" },
    Country { iso: "PM", name: "Saint Pierre and Miquelon", dial: "508" },
    Country { iso: "VC", name: "Saint Vincent and the Grenadines", dial: "1" },
    Country { iso: "WS", name: "Samoa", dial: "685" },
    Country { iso: "SM", name: "San Marino", dial: "378" },
    Country { iso: "ST", name: "Sao Tome and Principe", dial: "239" },
    Country { iso: "SA", name: "Saudi Arabia", dial: "966" },
    Country { iso: "SN", name: "Senegal", dial: "221" },
    Country { iso: "RS", name: "Serbia", dial: "381" },
    Country { iso: "SC", name: "Seychelles", dial: "248" },
    Country { iso: "SL", name: "Sierra Leone", dial: "232" },
    Country { iso: "SG", name: "Singapore", dial: "65" },
    Country { iso: "SK", name: "Slovakia", dial: "421" },
    Country { iso: "SI", name: "Slovenia", dial: "386" },
    Country { iso: "SB", name: "Solomon Islands", dial: "677" },
    Country { iso: "SO", name: "Somalia", dial: "252" },
    Country { iso: "ZA", name: "South Africa", dial: "27" },
    Country { iso: "KR", name: "South Korea", dial: "82" },
    Country { iso: "SS", name: "South Sudan", dial: "211" },
    Country { iso: "ES", name: "Spain", dial: "34" },
    Country { iso: "LK", name: "Sri Lanka", dial: "94" },
    Country { iso: "SD", name: "Sudan", dial: "249" },
    Country { iso: "SR", name: "Suriname", dial: "597" },
    Country { iso: "SE", name: "Sweden", dial: "46" },
    Country { iso: "CH", name: "Switzerland", dial: "41" },
    Country { iso: "SY", name: "Syria", dial: "963" },
    Country { iso: "TW", name: "Taiwan", dial: "886" },
    Country { iso: "TJ", name: "Tajikistan", dial: "992" },
    Country { iso: "TZ", name: "Tanzania", dial: "255" },
    Country { iso: "TH", name: "Thailand", dial: "66" },
    Country { iso: "TL", name: "Timor-Leste", dial: "670" },
    Country { iso: "TG", name: "Togo", dial: "228" },
    Country { iso: "TO", name: "Tonga", dial: "676" },
    Country { iso: "TT", name: "Trinidad and Tobago", dial: "1" },
    Country { iso: "TN", name: "Tunisia", dial: "216" },
    Country { iso: "TR", name: "Turkey", dial: "90" },
    Country { iso: "TM", name: "Turkmenistan", dial: "993" },
    Country { iso: "TC", name: "Turks and Caicos", dial: "1" },
    Country { iso: "TV", name: "Tuvalu", dial: "688" },
    Country { iso: "UG", name: "Uganda", dial: "256" },
    Country { iso: "UA", name: "Ukraine", dial: "380" },
    Country { iso: "AE", name: "United Arab Emirates", dial: "971" },
    Country { iso: "GB", name: "United Kingdom", dial: "44" },
    Country { iso: "US", name: "United States", dial: "1" },
    Country { iso: "UY", name: "Uruguay", dial: "598" },
    Country { iso: "VI", name: "US Virgin Islands", dial: "1" },
    Country { iso: "UZ", name: "Uzbekistan", dial: "998" },
    Country { iso: "VU", name: "Vanuatu", dial: "678" },
    Country { iso: "VA", name: "Vatican City", dial: "39" },
    Country { iso: "VE", name: "Venezuela", dial: "58" },
    Country { iso: "VN", name: "Vietnam", dial: "84" },
    Country { iso: "WF", name: "Wallis and Futuna", dial: "681" },
    Country { iso: "YE", name: "Yemen", dial: "967" },
    Country { iso: "ZM", name: "Zambia", dial: "260" },
    Country { iso: "ZW", name: "Zimbabwe", dial: "263" },
];

/// The country for an ISO alpha-2 code (case-insensitive).
pub(crate) fn by_iso(iso: &str) -> Option<&'static Country> {
    COUNTRIES.iter().find(|c| c.iso.eq_ignore_ascii_case(iso))
}

/// The flag for an ISO code: the regional-indicator pair
/// (U+1F1E6..) the color emoji font turns into the flag glyph.
pub(crate) fn flag_emoji(iso: &str) -> String {
    iso.chars()
        .filter_map(|c| {
            let upper = c.to_ascii_uppercase();
            upper.is_ascii_uppercase().then(|| {
                char::from_u32(0x1F1E6 + u32::from(upper) - u32::from('A')).unwrap_or(upper)
            })
        })
        .collect()
}

/// The region subtag of a locale string (`id_ID.UTF-8` ->
/// `ID`, `en-US` -> `US`); `None` for the POSIX/C locale and
/// anything without a region.
pub(crate) fn region_from_locale(value: &str) -> Option<String> {
    let head = value.split(['.', '@']).next()?;
    let region = head.rsplit(['_', '-']).next()?;
    // No separator means the string was a bare language
    // (`id`, `en`), which names no region at all.
    if region.len() == head.len() {
        return None;
    }
    (region.len() == 2 && region.chars().all(|c| c.is_ascii_alphabetic()))
        .then(|| region.to_ascii_uppercase())
}

/// The country the device locale names, from the standard
/// locale environment (LC_ALL, then LC_CTYPE, then LANG).
pub(crate) fn locale_country() -> Option<&'static Country> {
    for var in ["LC_ALL", "LC_CTYPE", "LANG"] {
        if let Ok(value) = std::env::var(var) {
            if let Some(region) = region_from_locale(&value) {
                if let Some(country) = by_iso(&region) {
                    return Some(country);
                }
            }
        }
    }
    None
}

/// The picker's preselected country: the device locale's
/// country, and where the locale names none, Indonesia. The
/// fallback is a starting selection the user can change in one
/// tap, never a lock-in; the old form's hardcoded `+1` hint is
/// the failure this replaces.
pub(crate) fn default_country() -> &'static Country {
    locale_country().unwrap_or_else(|| by_iso("ID").expect("Indonesia is in the table"))
}

/// Whether a country matches the picker's search text: by name
/// substring, ISO code, or dial code prefix (so "62", "+62",
/// "indo", and "ID" all find Indonesia). An empty query matches
/// everything.
pub(crate) fn matches(country: &Country, query: &str) -> bool {
    let q = query.trim().trim_start_matches('+').to_lowercase();
    q.is_empty()
        || country.name.to_lowercase().contains(&q)
        || country.iso.to_lowercase() == q
        || country.dial.starts_with(&q)
}

/// Composes the full international number the pairing request
/// consumes (`pair_phone`): `+`, the dial code, and the national
/// digits. Two typed forms are normalized, the way the phone
/// apps do: a pasted full number's repeated dial code is
/// dropped (when what follows still looks like a national
/// number), and one leading trunk zero is dropped (`0812...`
/// is not part of the international form).
pub(crate) fn compose_number(dial: &str, national: &str) -> String {
    let mut digits: String = national.chars().filter(char::is_ascii_digit).collect();
    if digits.len() > dial.len() + 6 && digits.starts_with(dial) {
        digits.drain(..dial.len());
    }
    let digits = digits.strip_prefix('0').unwrap_or(&digits);
    format!("+{dial}{digits}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_table_is_sorted_unique_and_complete() {
        assert!(COUNTRIES.len() > 200, "the table covers the world: {}", COUNTRIES.len());
        for pair in COUNTRIES.windows(2) {
            // Alphabetical as a reader scans it (case-insensitive):
            // byte order would file "US Virgin Islands" before
            // "Uganda", which is not the order the picker shows.
            assert!(
                pair[0].name.to_lowercase() < pair[1].name.to_lowercase(),
                "sorted by name: {} < {}",
                pair[0].name,
                pair[1].name
            );
            assert_ne!(pair[0].iso, pair[1].iso, "ISO codes are unique");
        }
        for c in COUNTRIES {
            assert_eq!(c.iso.len(), 2, "{}", c.name);
            assert!(!c.dial.is_empty() && c.dial.chars().all(|d| d.is_ascii_digit()), "{}", c.name);
        }
    }

    #[test]
    fn indonesia_is_plus_62() {
        let id = by_iso("ID").expect("Indonesia is in the table");
        assert_eq!(id.dial, "62");
        // Lookup is case-insensitive and lands on the same row.
        assert_eq!(by_iso("id").map(|c| c.dial), Some("62"));
        assert!(std::ptr::eq(by_iso("id").unwrap(), id));
    }

    #[test]
    fn flags_are_regional_indicator_pairs() {
        assert_eq!(flag_emoji("ID"), "\u{1F1EE}\u{1F1E9}");
        assert_eq!(flag_emoji("us"), "\u{1F1FA}\u{1F1F8}");
        assert_eq!(flag_emoji(""), "");
    }

    #[test]
    fn locale_regions_parse() {
        assert_eq!(region_from_locale("id_ID.UTF-8").as_deref(), Some("ID"));
        assert_eq!(region_from_locale("en_US.UTF-8").as_deref(), Some("US"));
        assert_eq!(region_from_locale("en-US").as_deref(), Some("US"));
        assert_eq!(region_from_locale("id_ID@latin").as_deref(), Some("ID"));
        assert_eq!(region_from_locale("C"), None);
        assert_eq!(region_from_locale(""), None);
        assert_eq!(region_from_locale("id"), None);
    }

    #[test]
    fn search_finds_by_name_code_and_dial() {
        let id = by_iso("ID").unwrap();
        assert!(matches(id, ""));
        assert!(matches(id, "indo"));
        assert!(matches(id, "ID"));
        assert!(matches(id, "62"));
        assert!(matches(id, "+62"));
        assert!(!matches(id, "japan"));
        assert!(!matches(id, "63"));
    }

    #[test]
    fn compose_normalizes_typed_forms() {
        // Plain national form.
        assert_eq!(compose_number("62", "812 3456 7890"), "+6281234567890");
        // The trunk zero is dropped.
        assert_eq!(compose_number("62", "0812 3456 7890"), "+6281234567890");
        // A pasted full international number loses its repeated
        // dial code instead of doubling it.
        assert_eq!(compose_number("62", "+62 812 3456 7890"), "+6281234567890");
        assert_eq!(compose_number("1", "+1 (415) 555-2671"), "+14155552671");
        // Nothing typed yet: just the code, ready for digits.
        assert_eq!(compose_number("62", ""), "+62");
    }
}
