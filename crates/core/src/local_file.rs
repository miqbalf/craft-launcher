//! Files on this computer opened in their Craft app ("Open local file…"). No I/O.
//!
//! A plain open: the file is not uploaded, locked or shared. The types are the ones the office
//! apps declare and the desktop defaults give them (WordCraft for documents, GridCraft for
//! spreadsheets, DeckCraft for presentations, PdfCraft for PDF). `.odp` is left out: DeckCraft
//! has no reader for it yet.

/// The app for each group of extensions (lowercase), in the order the file picker lists them.
pub const LOCAL_TYPES: [(&str, &[&str]); 4] =
    [("wordcraft", &["docx", "doc", "odt"]), ("gridcraft", &["xlsx", "xls", "ods", "csv"]), ("deckcraft", &["pptx", "ppt"]), ("pdfcraft", &["pdf"])];

/// Every extension in [`LOCAL_TYPES`], for the picker's "All supported" filter.
pub fn all_extensions() -> impl Iterator<Item = &'static str> {
    LOCAL_TYPES.iter().flat_map(|(_, exts)| exts.iter().copied())
}

/// The catalog app that opens the local file at `path`, chosen by its extension (any letter
/// case). `None` for a type no Craft app opens, and for a file without an extension.
pub fn app_for(path: &str) -> Option<&'static str> {
    let ext = std::path::Path::new(path).extension()?.to_str()?.to_ascii_lowercase();
    LOCAL_TYPES.iter().find(|(_, exts)| exts.contains(&ext.as_str())).map(|(app, _)| *app)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_office_type_opens_in_its_app_in_any_letter_case() {
        assert_eq!(app_for("/home/a/Report.docx"), Some("wordcraft"));
        assert_eq!(app_for("/home/a/Budget.XLSX"), Some("gridcraft"));
        assert_eq!(app_for("/home/a/old.Xls"), Some("gridcraft"));
        assert_eq!(app_for("/home/a/Talk.PPTX"), Some("deckcraft"));
        assert_eq!(app_for("C:\\Users\\a\\scan.pdf"), Some("pdfcraft"));
    }

    #[test]
    fn unsupported_types_open_nothing() {
        assert_eq!(app_for("/home/a/photo.jpg"), None);
        // DeckCraft can't read ODP yet, so it is not offered (unlike the cloud's mapping).
        assert_eq!(app_for("/home/a/slides.odp"), None);
        assert_eq!(app_for("/home/a/README"), None);
        // The extension is the file's, not a folder's.
        assert_eq!(app_for("/home/a/report.docx/notes"), None);
        assert_eq!(app_for("/home/a/.pdf"), None);
    }

    #[test]
    fn the_all_supported_filter_lists_every_type_once() {
        let all: Vec<_> = all_extensions().collect();
        let mut unique = all.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(all.len(), unique.len());
        assert!(all.iter().all(|ext| app_for(&format!("x.{ext}")).is_some()));
    }
}
