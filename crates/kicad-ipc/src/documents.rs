use std::path::{Path, PathBuf};

use crate::{Error, Kicad, proto};

use proto::kiapi::common::commands::{GetOpenDocuments, GetOpenDocumentsResponse};
use proto::kiapi::common::types::DocumentType;

impl Kicad {
    /// Find and cache the open PCB document. Call once before any board op.
    pub fn open_board(&mut self) -> Result<(), Error> {
        let resp: GetOpenDocumentsResponse = self.call(&GetOpenDocuments {
            r#type: DocumentType::DoctypePcb as i32,
        })?;
        self.board_doc = resp.documents.into_iter().next();
        if self.board_doc.is_none() {
            return Err(Error::NoBoard);
        }
        Ok(())
    }

    /// Find and cache the open PCB document whose filename matches `board`.
    pub fn open_board_path(&mut self, board: &Path) -> Result<(), Error> {
        let resp: GetOpenDocumentsResponse = self.call(&GetOpenDocuments {
            r#type: DocumentType::DoctypePcb as i32,
        })?;
        self.board_doc = resp
            .documents
            .into_iter()
            .find(|doc| document_matches_board(doc, board));
        if self.board_doc.is_none() {
            return Err(Error::NoBoard);
        }
        Ok(())
    }
}

fn document_matches_board(
    doc: &proto::kiapi::common::types::DocumentSpecifier,
    board: &Path,
) -> bool {
    let Some(proto::kiapi::common::types::document_specifier::Identifier::BoardFilename(name)) =
        doc.identifier.as_ref()
    else {
        return false;
    };
    if board.file_name().and_then(|s| s.to_str()) != Some(name.as_str()) {
        return false;
    }
    let Some(project) = &doc.project else {
        return true;
    };
    if project.path.is_empty() {
        return true;
    }
    same_pathish(
        &PathBuf::from(&project.path),
        board.parent().unwrap_or_else(|| Path::new("")),
    )
}

fn same_pathish(a: &Path, b: &Path) -> bool {
    if let (Ok(a), Ok(b)) = (a.canonicalize(), b.canonicalize()) {
        return a == b;
    }
    a == b
}

#[cfg(test)]
mod tests {
    use super::document_matches_board;
    use crate::proto::kiapi::common::types::document_specifier::Identifier;
    use crate::proto::kiapi::common::types::{DocumentSpecifier, DocumentType, ProjectSpecifier};

    #[test]
    fn document_match_rejects_same_filename_different_project() {
        let doc = DocumentSpecifier {
            r#type: DocumentType::DoctypePcb as i32,
            identifier: Some(Identifier::BoardFilename("design.kicad_pcb".to_string())),
            project: Some(ProjectSpecifier {
                name: "other".to_string(),
                path: "/tmp/other_project".to_string(),
            }),
        };

        assert!(!document_matches_board(
            &doc,
            std::path::Path::new("/tmp/this_project/design.kicad_pcb")
        ));
    }

    #[test]
    fn document_match_accepts_matching_project_and_filename() {
        let doc = DocumentSpecifier {
            r#type: DocumentType::DoctypePcb as i32,
            identifier: Some(Identifier::BoardFilename("design.kicad_pcb".to_string())),
            project: Some(ProjectSpecifier {
                name: "this_project".to_string(),
                path: "/tmp/this_project".to_string(),
            }),
        };

        assert!(document_matches_board(
            &doc,
            std::path::Path::new("/tmp/this_project/design.kicad_pcb")
        ));
    }
}
