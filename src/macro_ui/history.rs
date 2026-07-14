use std::collections::VecDeque;

use crate::macro_ui::{EditorCommand, EditorDraft, EditorError, apply_editor_command};
use crate::ui_state::MacroCanvasLayout;

use super::canvas_layout::LayoutEdit;

pub const UI_HISTORY_LIMIT: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditDomain {
    Definition,
    Layout,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HistoryError {
    NothingToUndo,
    NothingToRedo,
    Definition(EditorError),
    Layout,
}

#[derive(Debug, Default)]
pub struct LayoutHistory {
    undo: VecDeque<LayoutEdit>,
    redo: VecDeque<LayoutEdit>,
}

impl LayoutHistory {
    pub fn record(&mut self, edit: LayoutEdit) {
        if self.undo.len() == UI_HISTORY_LIMIT {
            self.undo.pop_front();
        }
        self.undo.push_back(edit);
        self.redo.clear();
    }

    pub fn undo(&mut self, layout: &mut MacroCanvasLayout) -> Result<(), HistoryError> {
        let edit = self.undo.pop_back().ok_or(HistoryError::NothingToUndo)?;
        edit.apply_before(layout);
        push_bounded(&mut self.redo, edit);
        Ok(())
    }

    pub fn redo(&mut self, layout: &mut MacroCanvasLayout) -> Result<(), HistoryError> {
        let edit = self.redo.pop_back().ok_or(HistoryError::NothingToRedo)?;
        edit.apply_after(layout);
        push_bounded(&mut self.undo, edit);
        Ok(())
    }
}

#[derive(Debug, Default)]
pub struct UiEditHistory {
    undo_domains: VecDeque<EditDomain>,
    redo_domains: VecDeque<EditDomain>,
    layout: LayoutHistory,
}

impl UiEditHistory {
    pub fn record_definition(&mut self) {
        self.record_domain(EditDomain::Definition);
    }

    pub fn record_layout(&mut self, edit: LayoutEdit) {
        self.layout.record(edit);
        self.record_domain(EditDomain::Layout);
    }

    pub fn undo(
        &mut self,
        draft: &mut EditorDraft,
        layout: &mut MacroCanvasLayout,
    ) -> Result<EditDomain, HistoryError> {
        let domain = self
            .undo_domains
            .pop_back()
            .ok_or(HistoryError::NothingToUndo)?;
        let result = match domain {
            EditDomain::Definition => apply_editor_command(draft, EditorCommand::Undo)
                .map(|_| ())
                .map_err(HistoryError::Definition),
            EditDomain::Layout => self.layout.undo(layout),
        };
        if result.is_err() {
            self.undo_domains.push_back(domain);
            return result.map(|_| domain);
        }
        push_bounded(&mut self.redo_domains, domain);
        Ok(domain)
    }

    pub fn redo(
        &mut self,
        draft: &mut EditorDraft,
        layout: &mut MacroCanvasLayout,
    ) -> Result<EditDomain, HistoryError> {
        let domain = self
            .redo_domains
            .pop_back()
            .ok_or(HistoryError::NothingToRedo)?;
        let result = match domain {
            EditDomain::Definition => apply_editor_command(draft, EditorCommand::Redo)
                .map(|_| ())
                .map_err(HistoryError::Definition),
            EditDomain::Layout => self.layout.redo(layout),
        };
        if result.is_err() {
            self.redo_domains.push_back(domain);
            return result.map(|_| domain);
        }
        push_bounded(&mut self.undo_domains, domain);
        Ok(domain)
    }

    pub fn undo_len(&self) -> usize {
        self.undo_domains.len()
    }
    pub fn redo_len(&self) -> usize {
        self.redo_domains.len()
    }

    fn record_domain(&mut self, domain: EditDomain) {
        push_bounded(&mut self.undo_domains, domain);
        self.redo_domains.clear();
    }
}

fn push_bounded<T>(history: &mut VecDeque<T>, entry: T) {
    if history.len() == UI_HISTORY_LIMIT {
        history.pop_front();
    }
    history.push_back(entry);
}

#[cfg(test)]
mod tests {
    use crate::macro_ui::canvas_layout::CanvasLayoutEngine;
    use crate::ui_state::MacroCanvasLayout;

    use super::*;

    #[test]
    fn layout_history_restores_node_move() {
        let mut layout = MacroCanvasLayout::default();
        let mut history = LayoutHistory::default();
        let edit = CanvasLayoutEngine::move_node(&mut layout, "observe", [20.0, 40.0]).unwrap();
        history.record(edit);
        history.undo(&mut layout).unwrap();
        assert!(!layout.node_positions.contains_key("observe"));
        history.redo(&mut layout).unwrap();
        assert_eq!(layout.node_positions["observe"], [20.0, 40.0]);
    }
}
