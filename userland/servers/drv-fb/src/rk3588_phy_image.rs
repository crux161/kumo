//j609

//! Orange Pi PHY0 image measured read-only on 2026-10-25, before any PHY reset/write.
//! Offsets and writable masks: RK3588 TRM Part 2 ch. 27 register descriptions. Values:
//! build/verification/j609/metal-snapshot.log; no vendor driver table is used. Only the
//! documented RW fields are replayed; all other bits retain the post-reset value. — KESTREL

#[derive(Clone, Copy, Debug)]
pub struct Word {
    pub offset: u32,
    pub mask: u32,
    pub value: u32,
}

pub const IMAGE: [Word; 109] = [
    Word {
        offset: 0x0000,
        mask: 0xff,
        value: 0x04,
    },
    Word {
        offset: 0x0020,
        mask: 0xff,
        value: 0x00,
    },
    Word {
        offset: 0x0024,
        mask: 0x3f,
        value: 0x0c,
    },
    Word {
        offset: 0x0028,
        mask: 0xff,
        value: 0x83,
    },
    Word {
        offset: 0x002c,
        mask: 0x0f,
        value: 0x06,
    },
    Word {
        offset: 0x0080,
        mask: 0xff,
        value: 0x00,
    },
    Word {
        offset: 0x0084,
        mask: 0xff,
        value: 0x00,
    },
    Word {
        offset: 0x0094,
        mask: 0x7f,
        value: 0x53,
    },
    Word {
        offset: 0x00a0,
        mask: 0x1f,
        value: 0x01,
    },
    Word {
        offset: 0x00b4,
        mask: 0x7f,
        value: 0x00,
    },
    Word {
        offset: 0x00b8,
        mask: 0x3f,
        value: 0x04,
    },
    Word {
        offset: 0x00bc,
        mask: 0xff,
        value: 0x00,
    },
    Word {
        offset: 0x00f0,
        mask: 0xff,
        value: 0x80,
    },
    Word {
        offset: 0x00f4,
        mask: 0xff,
        value: 0x40,
    },
    Word {
        offset: 0x0138,
        mask: 0xff,
        value: 0x34,
    },
    Word {
        offset: 0x0144,
        mask: 0xff,
        value: 0x87,
    },
    Word {
        offset: 0x014c,
        mask: 0xff,
        value: 0x7d,
    },
    Word {
        offset: 0x0154,
        mask: 0xff,
        value: 0x87,
    },
    Word {
        offset: 0x0168,
        mask: 0xff,
        value: 0x50,
    },
    Word {
        offset: 0x0170,
        mask: 0xff,
        value: 0x25,
    },
    Word {
        offset: 0x0178,
        mask: 0x7f,
        value: 0x00,
    },
    Word {
        offset: 0x0180,
        mask: 0xff,
        value: 0x09,
    },
    Word {
        offset: 0x018c,
        mask: 0xff,
        value: 0x4e,
    },
    Word {
        offset: 0x0190,
        mask: 0x0f,
        value: 0x07,
    },
    Word {
        offset: 0x0194,
        mask: 0xff,
        value: 0x05,
    },
    Word {
        offset: 0x01a4,
        mask: 0x3f,
        value: 0x00,
    },
    Word {
        offset: 0x01b0,
        mask: 0x3f,
        value: 0x14,
    },
    Word {
        offset: 0x01c0,
        mask: 0x3f,
        value: 0x18,
    },
    Word {
        offset: 0x01d0,
        mask: 0x0f,
        value: 0x04,
    },
    Word {
        offset: 0x0204,
        mask: 0xff,
        value: 0x01,
    },
    Word {
        offset: 0x0208,
        mask: 0xff,
        value: 0x04,
    },
    Word {
        offset: 0x020c,
        mask: 0x7f,
        value: 0x24,
    },
    Word {
        offset: 0x0210,
        mask: 0x7f,
        value: 0x20,
    },
    Word {
        offset: 0x0214,
        mask: 0x3f,
        value: 0x03,
    },
    Word {
        offset: 0x0218,
        mask: 0xff,
        value: 0x51,
    },
    Word {
        offset: 0x021c,
        mask: 0x0f,
        value: 0x04,
    },
    Word {
        offset: 0x0224,
        mask: 0x3f,
        value: 0x00,
    },
    Word {
        offset: 0x0254,
        mask: 0x3f,
        value: 0x00,
    },
    Word {
        offset: 0x025c,
        mask: 0xff,
        value: 0x02,
    },
    Word {
        offset: 0x0264,
        mask: 0xff,
        value: 0x04,
    },
    Word {
        offset: 0x0268,
        mask: 0xff,
        value: 0x11,
    },
    Word {
        offset: 0x026c,
        mask: 0x1f,
        value: 0x00,
    },
    Word {
        offset: 0x0278,
        mask: 0xff,
        value: 0x00,
    },
    Word {
        offset: 0x027c,
        mask: 0xff,
        value: 0x00,
    },
    Word {
        offset: 0x0280,
        mask: 0x7f,
        value: 0x00,
    },
    Word {
        offset: 0x0800,
        mask: 0x7f,
        value: 0x06,
    },
    Word {
        offset: 0x0804,
        mask: 0xff,
        value: 0x07,
    },
    Word {
        offset: 0x080c,
        mask: 0xff,
        value: 0xf0,
    },
    Word {
        offset: 0x0810,
        mask: 0xff,
        value: 0x7c,
    },
    Word {
        offset: 0x0814,
        mask: 0xff,
        value: 0x1f,
    },
    Word {
        offset: 0x0818,
        mask: 0xff,
        value: 0x07,
    },
    Word {
        offset: 0x081c,
        mask: 0x0f,
        value: 0x0f,
    },
    Word {
        offset: 0x0c0c,
        mask: 0x7f,
        value: 0x2f,
    },
    Word {
        offset: 0x0c1c,
        mask: 0x7f,
        value: 0x20,
    },
    Word {
        offset: 0x0c28,
        mask: 0x1f,
        value: 0x17,
    },
    Word {
        offset: 0x0c2c,
        mask: 0xff,
        value: 0x77,
    },
    Word {
        offset: 0x0c30,
        mask: 0xff,
        value: 0x77,
    },
    Word {
        offset: 0x0c34,
        mask: 0xff,
        value: 0x77,
    },
    Word {
        offset: 0x0c38,
        mask: 0x7f,
        value: 0x38,
    },
    Word {
        offset: 0x0c3c,
        mask: 0xff,
        value: 0x00,
    },
    Word {
        offset: 0x0c40,
        mask: 0xff,
        value: 0x03,
    },
    Word {
        offset: 0x0c44,
        mask: 0xff,
        value: 0x0f,
    },
    Word {
        offset: 0x0c48,
        mask: 0xff,
        value: 0x00,
    },
    Word {
        offset: 0x0c58,
        mask: 0x0f,
        value: 0x02,
    },
    Word {
        offset: 0x0c6c,
        mask: 0xff,
        value: 0x01,
    },
    Word {
        offset: 0x0c7c,
        mask: 0x3f,
        value: 0x15,
    },
    Word {
        offset: 0x0c80,
        mask: 0xff,
        value: 0xa0,
    },
    Word {
        offset: 0x100c,
        mask: 0x7f,
        value: 0x2f,
    },
    Word {
        offset: 0x101c,
        mask: 0x7f,
        value: 0x20,
    },
    Word {
        offset: 0x1028,
        mask: 0x1f,
        value: 0x17,
    },
    Word {
        offset: 0x102c,
        mask: 0xff,
        value: 0x77,
    },
    Word {
        offset: 0x1030,
        mask: 0xff,
        value: 0x77,
    },
    Word {
        offset: 0x1034,
        mask: 0xff,
        value: 0x77,
    },
    Word {
        offset: 0x1038,
        mask: 0x7f,
        value: 0x38,
    },
    Word {
        offset: 0x1040,
        mask: 0xff,
        value: 0x03,
    },
    Word {
        offset: 0x1044,
        mask: 0xff,
        value: 0x0f,
    },
    Word {
        offset: 0x1048,
        mask: 0xff,
        value: 0x00,
    },
    Word {
        offset: 0x1058,
        mask: 0x0f,
        value: 0x02,
    },
    Word {
        offset: 0x106c,
        mask: 0xff,
        value: 0x01,
    },
    Word {
        offset: 0x107c,
        mask: 0x3f,
        value: 0x15,
    },
    Word {
        offset: 0x140c,
        mask: 0x7f,
        value: 0x2f,
    },
    Word {
        offset: 0x141c,
        mask: 0x7f,
        value: 0x20,
    },
    Word {
        offset: 0x1428,
        mask: 0x1f,
        value: 0x17,
    },
    Word {
        offset: 0x142c,
        mask: 0xff,
        value: 0x77,
    },
    Word {
        offset: 0x1430,
        mask: 0xff,
        value: 0x77,
    },
    Word {
        offset: 0x1434,
        mask: 0x1f,
        value: 0x77,
    },
    Word {
        offset: 0x1438,
        mask: 0x7f,
        value: 0x38,
    },
    Word {
        offset: 0x1440,
        mask: 0xff,
        value: 0x03,
    },
    Word {
        offset: 0x1444,
        mask: 0xff,
        value: 0x0f,
    },
    Word {
        offset: 0x1448,
        mask: 0xff,
        value: 0x00,
    },
    Word {
        offset: 0x1458,
        mask: 0x0f,
        value: 0x02,
    },
    Word {
        offset: 0x146c,
        mask: 0x01,
        value: 0x01,
    },
    Word {
        offset: 0x147c,
        mask: 0x3f,
        value: 0x15,
    },
    Word {
        offset: 0x1480,
        mask: 0xff,
        value: 0xa0,
    },
    Word {
        offset: 0x1484,
        mask: 0xff,
        value: 0x14,
    },
    Word {
        offset: 0x180c,
        mask: 0x7f,
        value: 0x2f,
    },
    Word {
        offset: 0x181c,
        mask: 0x7f,
        value: 0x20,
    },
    Word {
        offset: 0x1828,
        mask: 0x1f,
        value: 0x17,
    },
    Word {
        offset: 0x182c,
        mask: 0xff,
        value: 0x77,
    },
    Word {
        offset: 0x1830,
        mask: 0xff,
        value: 0x77,
    },
    Word {
        offset: 0x1834,
        mask: 0xff,
        value: 0x77,
    },
    Word {
        offset: 0x1838,
        mask: 0x7f,
        value: 0x38,
    },
    Word {
        offset: 0x1840,
        mask: 0xff,
        value: 0x03,
    },
    Word {
        offset: 0x1844,
        mask: 0xff,
        value: 0x0f,
    },
    Word {
        offset: 0x1848,
        mask: 0xff,
        value: 0x00,
    },
    Word {
        offset: 0x1858,
        mask: 0x0f,
        value: 0x02,
    },
    Word {
        offset: 0x186c,
        mask: 0xff,
        value: 0x01,
    },
    Word {
        offset: 0x1878,
        mask: 0xff,
        value: 0x08,
    },
    Word {
        offset: 0x187c,
        mask: 0x3f,
        value: 0x15,
    },
];
