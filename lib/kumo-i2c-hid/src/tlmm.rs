use crate::{I2c21PinctrlTopology, TlmmFunction, TlmmOutput, TlmmPinctrlGroup};

pub const SC8280XP_TLMM_GPIO_STRIDE: u32 = 0x1000;
pub const SC8280XP_TLMM_GPIO_CTL_OFFSET: u32 = 0x000;
pub const SC8280XP_TLMM_GPIO_IO_OFFSET: u32 = 0x004;
pub const MAX_TLMM_PINCTRL_UPDATES: usize =
    crate::MAX_TLMM_PINCTRL_GROUPS * crate::MAX_TLMM_PINS_PER_GROUP * 4;

const SC8280XP_LAST_GPIO: u16 = 227;
const SC8280XP_MUX_BIT: u32 = 2;
const SC8280XP_MUX_MASK: u32 = 0b111 << SC8280XP_MUX_BIT;
const SC8280XP_PULL_BIT: u32 = 0;
const SC8280XP_PULL_MASK: u32 = 0b11 << SC8280XP_PULL_BIT;
const SC8280XP_DRIVE_BIT: u32 = 6;
const SC8280XP_DRIVE_MASK: u32 = 0b111 << SC8280XP_DRIVE_BIT;
const SC8280XP_OE_BIT: u32 = 9;
const SC8280XP_OE_MASK: u32 = 1 << SC8280XP_OE_BIT;
const SC8280XP_OUT_BIT: u32 = 1;
const SC8280XP_OUT_MASK: u32 = 1 << SC8280XP_OUT_BIT;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TlmmRegisterUpdate {
    pub offset: u32,
    pub clear_mask: u32,
    pub set_mask: u32,
}

impl TlmmRegisterUpdate {
    const EMPTY: Self = Self {
        offset: 0,
        clear_mask: 0,
        set_mask: 0,
    };

    pub const fn apply(self, existing: u32) -> u32 {
        (existing & !self.clear_mask) | self.set_mask
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TlmmPinctrlPlan {
    updates: [TlmmRegisterUpdate; MAX_TLMM_PINCTRL_UPDATES],
    update_count: usize,
}

impl TlmmPinctrlPlan {
    const EMPTY: Self = Self {
        updates: [TlmmRegisterUpdate::EMPTY; MAX_TLMM_PINCTRL_UPDATES],
        update_count: 0,
    };

    pub fn updates(&self) -> &[TlmmRegisterUpdate] {
        &self.updates[..self.update_count]
    }

    fn push(&mut self, update: TlmmRegisterUpdate) -> Result<(), TlmmPinctrlPlanError> {
        if self.update_count == self.updates.len() {
            return Err(TlmmPinctrlPlanError::TooManyUpdates);
        }
        self.updates[self.update_count] = update;
        self.update_count += 1;
        Ok(())
    }
}

impl Default for TlmmPinctrlPlan {
    fn default() -> Self {
        Self::EMPTY
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TlmmPinctrlPlanError {
    UnsupportedPin,
    UnsupportedFunction,
    InvalidDriveStrength,
    TooManyUpdates,
}

pub trait TlmmRegisterIo {
    fn read(&mut self, offset: u32) -> u32;
    fn write(&mut self, offset: u32, value: u32);
}

pub fn sc8280xp_i2c21_tlmm_plan(
    topology: &I2c21PinctrlTopology,
) -> Result<TlmmPinctrlPlan, TlmmPinctrlPlanError> {
    let mut plan = TlmmPinctrlPlan::EMPTY;
    for group in topology.groups() {
        for pin in group.pins() {
            sc8280xp_plan_pin(&mut plan, *pin, group)?;
        }
    }
    Ok(plan)
}

pub fn apply_tlmm_pinctrl_plan(io: &mut impl TlmmRegisterIo, plan: &TlmmPinctrlPlan) -> usize {
    for update in plan.updates() {
        let existing = io.read(update.offset);
        io.write(update.offset, update.apply(existing));
    }
    plan.updates().len()
}

fn sc8280xp_plan_pin(
    plan: &mut TlmmPinctrlPlan,
    pin: u16,
    group: &TlmmPinctrlGroup,
) -> Result<(), TlmmPinctrlPlanError> {
    if pin > SC8280XP_LAST_GPIO {
        return Err(TlmmPinctrlPlanError::UnsupportedPin);
    }
    plan.push(sc8280xp_mux_update(pin, group.function)?)?;
    if group.bias_disable {
        plan.push(sc8280xp_ctl_update(pin, SC8280XP_PULL_MASK, 0))?;
    }
    if let Some(strength_ma) = group.drive_strength {
        plan.push(sc8280xp_drive_update(pin, strength_ma)?)?;
    }
    match group.output {
        TlmmOutput::None => {}
        TlmmOutput::Low => {
            plan.push(sc8280xp_io_update(pin, SC8280XP_OUT_MASK, 0))?;
            plan.push(sc8280xp_ctl_update(pin, SC8280XP_OE_MASK, SC8280XP_OE_MASK))?;
        }
        TlmmOutput::High => {
            plan.push(sc8280xp_io_update(
                pin,
                SC8280XP_OUT_MASK,
                SC8280XP_OUT_MASK,
            ))?;
            plan.push(sc8280xp_ctl_update(pin, SC8280XP_OE_MASK, SC8280XP_OE_MASK))?;
        }
    }
    Ok(())
}

fn sc8280xp_mux_update(
    pin: u16,
    function: TlmmFunction,
) -> Result<TlmmRegisterUpdate, TlmmPinctrlPlanError> {
    let mux = match function {
        TlmmFunction::Gpio => 0,
        TlmmFunction::Qup21 if (81..=84).contains(&pin) => 1,
        _ => return Err(TlmmPinctrlPlanError::UnsupportedFunction),
    };
    Ok(sc8280xp_ctl_update(
        pin,
        SC8280XP_MUX_MASK,
        mux << SC8280XP_MUX_BIT,
    ))
}

fn sc8280xp_drive_update(
    pin: u16,
    strength_ma: u8,
) -> Result<TlmmRegisterUpdate, TlmmPinctrlPlanError> {
    if strength_ma < 2 || strength_ma > 16 || strength_ma % 2 != 0 {
        return Err(TlmmPinctrlPlanError::InvalidDriveStrength);
    }
    Ok(sc8280xp_ctl_update(
        pin,
        SC8280XP_DRIVE_MASK,
        ((strength_ma as u32 / 2) - 1) << SC8280XP_DRIVE_BIT,
    ))
}

fn sc8280xp_ctl_update(pin: u16, clear_mask: u32, set_mask: u32) -> TlmmRegisterUpdate {
    TlmmRegisterUpdate {
        offset: pin as u32 * SC8280XP_TLMM_GPIO_STRIDE + SC8280XP_TLMM_GPIO_CTL_OFFSET,
        clear_mask,
        set_mask,
    }
}

fn sc8280xp_io_update(pin: u16, clear_mask: u32, set_mask: u32) -> TlmmRegisterUpdate {
    TlmmRegisterUpdate {
        offset: pin as u32 * SC8280XP_TLMM_GPIO_STRIDE + SC8280XP_TLMM_GPIO_IO_OFFSET,
        clear_mask,
        set_mask,
    }
}

#[cfg(test)]
mod tests {
    extern crate alloc;

    use alloc::{collections::BTreeMap, vec::Vec};

    use super::*;
    use crate::discover_i2c21_pinctrl;

    const X13S_DTB: &[u8] = include_bytes!("../../../sc8280xp-lenovo-thinkpad-x13s.dtb");

    const fn update(offset: u32, clear_mask: u32, set_mask: u32) -> TlmmRegisterUpdate {
        TlmmRegisterUpdate {
            offset,
            clear_mask,
            set_mask,
        }
    }

    #[derive(Default)]
    struct FakeTlmm {
        values: BTreeMap<u32, u32>,
        writes: Vec<(u32, u32)>,
    }

    impl TlmmRegisterIo for FakeTlmm {
        fn read(&mut self, offset: u32) -> u32 {
            self.values.get(&offset).copied().unwrap_or(0)
        }

        fn write(&mut self, offset: u32, value: u32) {
            self.values.insert(offset, value);
            self.writes.push((offset, value));
        }
    }

    #[test]
    fn register_update_applies_clear_then_set_masks() {
        let update = TlmmRegisterUpdate {
            offset: 0x51000,
            clear_mask: 0x1f,
            set_mask: 0x04,
        };
        assert_eq!(update.apply(0xffff_ffff), 0xffff_ffe4);
    }

    #[test]
    fn real_x13s_i2c21_pinctrl_plans_linux_equivalent_tlmm_updates() {
        let topology = discover_i2c21_pinctrl(X13S_DTB).expect("X13s i2c21 pinctrl");
        let plan = sc8280xp_i2c21_tlmm_plan(&topology).expect("SC8280XP TLMM plan");
        assert_eq!(
            plan.updates(),
            &[
                update(0x51000, 0x01c, 0x004),
                update(0x51000, 0x003, 0x000),
                update(0x51000, 0x1c0, 0x1c0),
                update(0x52000, 0x01c, 0x004),
                update(0x52000, 0x003, 0x000),
                update(0x52000, 0x1c0, 0x1c0),
                update(0x66000, 0x01c, 0x000),
                update(0x66004, 0x002, 0x000),
                update(0x66000, 0x200, 0x200),
                update(0x68000, 0x01c, 0x000),
                update(0x68000, 0x003, 0x000),
                update(0x69000, 0x01c, 0x000),
                update(0x69000, 0x003, 0x000),
                update(0xb6000, 0x01c, 0x000),
                update(0xb6000, 0x003, 0x000),
            ]
        );
    }

    #[test]
    fn applying_real_x13s_plan_reads_modifies_and_writes_in_order() {
        let topology = discover_i2c21_pinctrl(X13S_DTB).expect("X13s i2c21 pinctrl");
        let plan = sc8280xp_i2c21_tlmm_plan(&topology).expect("SC8280XP TLMM plan");
        let mut fake = FakeTlmm::default();

        assert_eq!(apply_tlmm_pinctrl_plan(&mut fake, &plan), 15);
        assert_eq!(
            fake.writes,
            &[
                (0x51000, 0x004),
                (0x51000, 0x004),
                (0x51000, 0x1c4),
                (0x52000, 0x004),
                (0x52000, 0x004),
                (0x52000, 0x1c4),
                (0x66000, 0x000),
                (0x66004, 0x000),
                (0x66000, 0x200),
                (0x68000, 0x000),
                (0x68000, 0x000),
                (0x69000, 0x000),
                (0x69000, 0x000),
                (0xb6000, 0x000),
                (0xb6000, 0x000),
            ]
        );
    }
}
