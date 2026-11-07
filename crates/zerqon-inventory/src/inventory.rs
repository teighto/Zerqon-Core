use crate::{
    ClickMode, ClickRequest, CreativeSlotRequest, ItemCatalog, ItemError, ItemStack, Slot,
};
use std::collections::BTreeSet;

pub const MENU_SLOT_COUNT: usize = 46;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InventoryPolicy {
    pub creative: bool,
    pub can_interact: bool,
    pub allow_drops: bool,
}

impl Default for InventoryPolicy {
    fn default() -> Self {
        Self {
            creative: false,
            can_interact: true,
            allow_drops: false,
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum InventoryError {
    #[error("inventory interaction is denied")]
    InteractionDenied,
    #[error("creative inventory action requires creative mode")]
    CreativeRequired,
    #[error("wrong inventory container")]
    WrongContainer,
    #[error("invalid inventory state ID")]
    InvalidStateId,
    #[error("invalid inventory slot: {0}")]
    InvalidSlot(i16),
    #[error("crafting and equipment slot behavior is not implemented: {0}")]
    UnsupportedSlot(i16),
    #[error("invalid button for inventory action")]
    InvalidButton,
    #[error("invalid quick-craft sequence")]
    InvalidDragSequence,
    #[error("item-specific inventory behavior is not implemented")]
    UnsupportedItemBehavior,
    #[error("item dropping is unavailable")]
    DroppingUnavailable,
    #[error("invalid changed-slot prediction")]
    InvalidPrediction,
    #[error(transparent)]
    Item(#[from] ItemError),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InventoryEffects {
    pub changed_slots: Vec<(i16, Slot)>,
    pub cursor: Slot,
    pub state_id: u16,
    pub drops: Vec<ItemStack>,
    pub force_full_sync: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DragState {
    kind: u8,
    slots: BTreeSet<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayerInventory {
    slots: [Slot; MENU_SLOT_COUNT],
    cursor: Slot,
    selected: u8,
    state_id: u16,
    drag: Option<DragState>,
}

impl Default for PlayerInventory {
    fn default() -> Self {
        Self {
            slots: std::array::from_fn(|_| None),
            cursor: None,
            selected: 0,
            state_id: 0,
            drag: None,
        }
    }
}

impl PlayerInventory {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn slots(&self) -> &[Slot; MENU_SLOT_COUNT] {
        &self.slots
    }
    pub fn cursor(&self) -> &Slot {
        &self.cursor
    }
    pub fn selected(&self) -> u8 {
        self.selected
    }
    pub fn state_id(&self) -> u16 {
        self.state_id
    }
    pub fn selected_stack(&self) -> &Slot {
        &self.slots[36 + usize::from(self.selected)]
    }

    pub fn set_slot(
        &mut self,
        catalog: &ItemCatalog,
        index: i16,
        stack: Slot,
    ) -> Result<(), InventoryError> {
        let index = menu_index(index)?;
        validate_slot(catalog, &stack)?;
        self.slots[index] = stack;
        self.drag = None;
        Ok(())
    }

    pub fn set_cursor(&mut self, catalog: &ItemCatalog, stack: Slot) -> Result<(), InventoryError> {
        validate_slot(catalog, &stack)?;
        self.cursor = stack;
        self.drag = None;
        Ok(())
    }

    pub fn select_hotbar(&mut self, slot: i16) -> Result<(), InventoryError> {
        if !(0..=8).contains(&slot) {
            return Err(InventoryError::InvalidSlot(slot));
        }
        self.selected = slot as u8;
        Ok(())
    }

    pub fn creative_set(
        &mut self,
        catalog: &ItemCatalog,
        request: &CreativeSlotRequest,
        policy: InventoryPolicy,
    ) -> Result<InventoryEffects, InventoryError> {
        if !policy.can_interact {
            return Err(InventoryError::InteractionDenied);
        }
        if !policy.creative {
            return Err(InventoryError::CreativeRequired);
        }
        validate_slot(catalog, &request.stack)?;
        let mut next = self.clone();
        let mut drops = Vec::new();
        if request.slot < 0 {
            if !policy.allow_drops {
                return Err(InventoryError::DroppingUnavailable);
            }
            if let Some(stack) = &request.stack {
                drops.push(stack.clone());
            }
        } else {
            if !(1..=45).contains(&request.slot) {
                return Err(InventoryError::InvalidSlot(request.slot));
            }
            next.slots[request.slot as usize] = request.stack.clone();
        }
        next.drag = None;
        Ok(self.commit(next, drops))
    }

    pub fn click(
        &mut self,
        catalog: &ItemCatalog,
        request: &ClickRequest,
        policy: InventoryPolicy,
    ) -> Result<InventoryEffects, InventoryError> {
        if !policy.can_interact {
            return Err(InventoryError::InteractionDenied);
        }
        if request.container_id != 0 {
            return Err(InventoryError::WrongContainer);
        }
        if !(0..=32767).contains(&request.state_id) {
            return Err(InventoryError::InvalidStateId);
        }
        validate_request(request)?;
        if request.mode == ClickMode::Clone && !policy.creative {
            return Err(InventoryError::CreativeRequired);
        }
        let mut next = self.clone();
        let mut drops = Vec::new();
        if request.mode != ClickMode::QuickCraft && next.drag.take().is_some() {
            return Ok(self.commit(next, drops));
        }
        match request.mode {
            ClickMode::Pickup => {
                next.pickup(catalog, request.slot, request.button, policy, &mut drops)?
            }
            ClickMode::QuickMove => next.quick_move(catalog, request.slot)?,
            ClickMode::Swap => next.swap(request.slot, request.button)?,
            ClickMode::Clone => next.clone_stack(catalog, request.slot, policy)?,
            ClickMode::Throw => next.throw(request.slot, request.button, policy, &mut drops)?,
            ClickMode::QuickCraft => {
                next.quick_craft(catalog, request.slot, request.button, policy, &mut drops)?
            }
            ClickMode::PickupAll => next.pickup_all(catalog, request.slot, request.button)?,
        }
        Ok(self.commit(next, drops))
    }

    fn commit(&mut self, mut next: Self, drops: Vec<ItemStack>) -> InventoryEffects {
        next.state_id = (self.state_id + 1) & 32767;
        let changed_slots = self
            .slots
            .iter()
            .zip(&next.slots)
            .enumerate()
            .filter(|(_, (before, after))| before != after)
            .map(|(index, (_, after))| (index as i16, after.clone()))
            .collect();
        let effects = InventoryEffects {
            changed_slots,
            cursor: next.cursor.clone(),
            state_id: next.state_id,
            drops,
            force_full_sync: true,
        };
        *self = next;
        effects
    }

    fn pickup(
        &mut self,
        catalog: &ItemCatalog,
        slot: i16,
        button: i8,
        policy: InventoryPolicy,
        drops: &mut Vec<ItemStack>,
    ) -> Result<(), InventoryError> {
        if slot == -999 {
            if self.cursor.is_some() && !policy.allow_drops {
                return Err(InventoryError::DroppingUnavailable);
            }
            if let Some(stack) = self.cursor.take() {
                let amount = if button == 0 { stack.count() } else { 1 };
                drops.push(stack.with_count(amount)?);
                self.cursor = quantity(&stack, stack.count() - amount)?;
            }
            return Ok(());
        }
        let index = interactive_index(slot)?;
        generic_slot(catalog, &self.cursor)?;
        generic_slot(catalog, &self.slots[index])?;
        match (self.slots[index].clone(), self.cursor.clone()) {
            (None, None) => {}
            (Some(target), None) => {
                let take = if button == 0 {
                    target.count()
                } else {
                    target.count().div_ceil(2)
                };
                self.slots[index] = quantity(&target, target.count() - take)?;
                self.cursor = quantity(&target, take)?;
            }
            (None, Some(cursor)) => {
                let put = if button == 0 { cursor.count() } else { 1 };
                self.slots[index] = quantity(&cursor, put)?;
                self.cursor = quantity(&cursor, cursor.count() - put)?;
            }
            (Some(target), Some(cursor)) if catalog.same_kind(&target, &cursor)? => {
                let maximum = catalog.effective_max_stack(&target)?;
                let room = maximum.saturating_sub(target.count());
                let put = cursor
                    .count()
                    .min(room)
                    .min(if button == 0 { u8::MAX } else { 1 });
                self.slots[index] = quantity(&target, target.count() + put)?;
                self.cursor = quantity(&cursor, cursor.count() - put)?;
            }
            (Some(target), Some(cursor)) => {
                self.slots[index] = Some(cursor);
                self.cursor = Some(target);
            }
        }
        Ok(())
    }

    fn quick_move(&mut self, catalog: &ItemCatalog, slot: i16) -> Result<(), InventoryError> {
        let source = interactive_index(slot)?;
        let Some(stack) = self.slots[source].clone() else {
            return Ok(());
        };
        if !catalog.supports_generic_quick_move(&stack)? {
            return Err(InventoryError::UnsupportedItemBehavior);
        }
        let destinations = match source {
            9..=35 => 36..45,
            36..=44 => 9..36,
            _ => 9..45,
        };
        let maximum = catalog.effective_max_stack(&stack)?;
        let mut remaining = stack.count();
        for index in destinations.clone() {
            let Some(target) = self.slots[index].as_ref() else {
                continue;
            };
            if !catalog.same_kind(target, &stack)? {
                continue;
            }
            let amount = remaining.min(maximum.saturating_sub(target.count()));
            if amount != 0 {
                self.slots[index] = quantity(target, target.count() + amount)?;
                remaining -= amount;
            }
            if remaining == 0 {
                break;
            }
        }
        if remaining != 0 {
            for index in destinations {
                if self.slots[index].is_none() {
                    self.slots[index] = quantity(&stack, remaining)?;
                    remaining = 0;
                    break;
                }
            }
        }
        self.slots[source] = quantity(&stack, remaining)?;
        Ok(())
    }

    fn swap(&mut self, slot: i16, button: i8) -> Result<(), InventoryError> {
        let source = interactive_index(slot)?;
        let target = if button == 40 {
            45
        } else {
            36 + button as usize
        };
        self.slots.swap(source, target);
        Ok(())
    }

    fn clone_stack(
        &mut self,
        catalog: &ItemCatalog,
        slot: i16,
        policy: InventoryPolicy,
    ) -> Result<(), InventoryError> {
        if !policy.creative {
            return Err(InventoryError::CreativeRequired);
        }
        let source = interactive_index(slot)?;
        if self.cursor.is_none()
            && let Some(stack) = &self.slots[source]
        {
            self.cursor = quantity(stack, catalog.effective_max_stack(stack)?)?;
        }
        Ok(())
    }

    fn throw(
        &mut self,
        slot: i16,
        button: i8,
        policy: InventoryPolicy,
        drops: &mut Vec<ItemStack>,
    ) -> Result<(), InventoryError> {
        let index = interactive_index(slot)?;
        if self.cursor.is_some() {
            return Ok(());
        }
        if let Some(stack) = self.slots[index].clone() {
            if !policy.allow_drops {
                return Err(InventoryError::DroppingUnavailable);
            }
            let amount = if button == 0 { 1 } else { stack.count() };
            self.slots[index] = quantity(&stack, stack.count() - amount)?;
            drops.push(stack.with_count(amount)?);
        }
        Ok(())
    }

    fn quick_craft(
        &mut self,
        catalog: &ItemCatalog,
        slot: i16,
        button: i8,
        policy: InventoryPolicy,
        drops: &mut Vec<ItemStack>,
    ) -> Result<(), InventoryError> {
        let stage = (button as u8) & 3;
        let kind = (button as u8) >> 2;
        if kind == 2 && !policy.creative {
            return Err(InventoryError::CreativeRequired);
        }
        let cursor = self
            .cursor
            .clone()
            .ok_or(InventoryError::InvalidDragSequence)?;
        generic_slot(catalog, &Some(cursor.clone()))?;
        match stage {
            0 => {
                if self.drag.is_some() {
                    return Err(InventoryError::InvalidDragSequence);
                }
                self.drag = Some(DragState {
                    kind,
                    slots: BTreeSet::new(),
                });
            }
            1 => {
                let index = interactive_index(slot)?;
                generic_slot(catalog, &self.slots[index])?;
                let maximum = catalog.effective_max_stack(&cursor)?;
                let eligible = match &self.slots[index] {
                    None => true,
                    Some(target) => {
                        catalog.same_kind(target, &cursor)? && target.count() <= maximum
                    }
                };
                let drag = self
                    .drag
                    .as_mut()
                    .filter(|drag| drag.kind == kind)
                    .ok_or(InventoryError::InvalidDragSequence)?;
                if eligible && (kind == 2 || usize::from(cursor.count()) > drag.slots.len()) {
                    drag.slots.insert(index);
                }
            }
            2 => {
                let drag = self
                    .drag
                    .take()
                    .filter(|drag| drag.kind == kind)
                    .ok_or(InventoryError::InvalidDragSequence)?;
                if drag.slots.len() == 1 {
                    if kind <= 1
                        && let Some(&index) = drag.slots.first()
                    {
                        self.pickup(catalog, index as i16, kind as i8, policy, drops)?;
                    }
                    return Ok(());
                }
                if drag.slots.is_empty() {
                    return Ok(());
                }
                let maximum = catalog.effective_max_stack(&cursor)?;
                let each = match kind {
                    0 => usize::from(cursor.count()) / drag.slots.len(),
                    1 => 1,
                    _ => usize::from(maximum),
                };
                let mut remaining = usize::from(cursor.count());
                for index in drag.slots {
                    let target = &self.slots[index];
                    generic_slot(catalog, target)?;
                    if let Some(target) = target
                        && !catalog.same_kind(target, &cursor)?
                    {
                        continue;
                    }
                    let previous = target.as_ref().map_or(0, ItemStack::count);
                    let amount = each.min(usize::from(maximum.saturating_sub(previous)));
                    self.slots[index] = quantity(&cursor, previous + amount as u8)?;
                    remaining = remaining.saturating_sub(amount);
                }
                self.cursor = quantity(&cursor, remaining as u8)?;
            }
            _ => return Err(InventoryError::InvalidButton),
        }
        Ok(())
    }

    fn pickup_all(
        &mut self,
        catalog: &ItemCatalog,
        slot: i16,
        button: i8,
    ) -> Result<(), InventoryError> {
        let index = interactive_index(slot)?;
        let Some(cursor) = self.cursor.clone() else {
            return Ok(());
        };
        generic_slot(catalog, &Some(cursor.clone()))?;
        if self.slots[index].is_some() {
            return Ok(());
        }
        let maximum = catalog.effective_max_stack(&cursor)?;
        if cursor.count() < maximum {
            for (index, slot) in self.slots.iter().enumerate().take(9).skip(1) {
                if let Some(stack) = slot
                    && catalog.same_kind(stack, &cursor)?
                {
                    return Err(InventoryError::UnsupportedSlot(index as i16));
                }
            }
        }
        let mut carried = cursor.count();
        let indexes: Vec<_> = if button == 0 {
            (9..MENU_SLOT_COUNT).collect()
        } else {
            (9..MENU_SLOT_COUNT).rev().collect()
        };
        for pass in 0..2 {
            for &index in &indexes {
                let Some(target) = self.slots[index].clone() else {
                    continue;
                };
                if !catalog.same_kind(&target, &cursor)? || (pass == 0 && target.count() == maximum)
                {
                    continue;
                }
                generic_slot(catalog, &Some(target.clone()))?;
                let amount = target.count().min(maximum - carried);
                self.slots[index] = quantity(&target, target.count() - amount)?;
                carried += amount;
                if carried == maximum {
                    break;
                }
            }
            if carried == maximum {
                break;
            }
        }
        self.cursor = quantity(&cursor, carried)?;
        Ok(())
    }
}

fn menu_index(slot: i16) -> Result<usize, InventoryError> {
    if !(0..MENU_SLOT_COUNT as i16).contains(&slot) {
        return Err(InventoryError::InvalidSlot(slot));
    }
    Ok(slot as usize)
}

fn interactive_index(slot: i16) -> Result<usize, InventoryError> {
    let index = menu_index(slot)?;
    if index < 9 {
        return Err(InventoryError::UnsupportedSlot(slot));
    }
    Ok(index)
}

fn validate_slot(catalog: &ItemCatalog, slot: &Slot) -> Result<(), InventoryError> {
    if let Some(stack) = slot {
        catalog.validate_stack(stack)?;
    }
    Ok(())
}

fn generic_slot(catalog: &ItemCatalog, slot: &Slot) -> Result<(), InventoryError> {
    if let Some(stack) = slot
        && !catalog.supports_generic_click(stack)?
    {
        return Err(InventoryError::UnsupportedItemBehavior);
    }
    Ok(())
}

fn quantity(stack: &ItemStack, count: u8) -> Result<Slot, InventoryError> {
    if count == 0 {
        Ok(None)
    } else {
        Ok(Some(stack.with_count(count)?))
    }
}

fn validate_request(request: &ClickRequest) -> Result<(), InventoryError> {
    let valid_button = match request.mode {
        ClickMode::Pickup | ClickMode::QuickMove | ClickMode::Throw | ClickMode::PickupAll => {
            (0..=1).contains(&request.button)
        }
        ClickMode::Swap => (0..=8).contains(&request.button) || request.button == 40,
        ClickMode::Clone => request.button == 2,
        ClickMode::QuickCraft => matches!(request.button, 0..=2 | 4..=6 | 8..=10),
    };
    if !valid_button {
        return Err(InventoryError::InvalidButton);
    }
    let outside_allowed = request.mode == ClickMode::Pickup
        || (request.mode == ClickMode::QuickCraft && request.button & 3 != 1);
    if request.slot != -999 || !outside_allowed {
        interactive_index(request.slot)?;
    }
    if request.changed_slots.len() > 128 {
        return Err(InventoryError::InvalidPrediction);
    }
    let mut seen = BTreeSet::new();
    for (slot, _) in &request.changed_slots {
        if menu_index(*slot).is_err() || !seen.insert(*slot) {
            return Err(InventoryError::InvalidPrediction);
        }
    }
    Ok(())
}
