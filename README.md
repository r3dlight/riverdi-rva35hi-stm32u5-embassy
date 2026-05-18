# riverdi-rva35hi-stm32u5-embassy

`no_std` ILI9488 driver for the **Riverdi RVA35HI-NUC144A** 3.5″
480×320 IPS LCD shield over the STM32U5 FMC NORSRAM bus in 8080
parallel mode.

- One-shot bring-up: reset pulse → FMC + GPIO + MPU → vendor init seq → ready.
- Implements [`embedded_graphics::draw_target::DrawTarget`] for `Rgb565`.
- Fast `clear` / `fill_rect` helpers that bypass the per-pixel path.
- Captures the three non-obvious hardware traps on this shield (see below).

## Scope

**Embassy-only.** The driver builds on `embassy-stm32` types
(`peripherals::PE4`, `gpio::Output`, `pac::RCC`) directly — it is
not a portable `embedded-hal-async` driver and won't work with
`stm32-hal2` or raw `stm32-metapac`. The `-embassy` suffix in the
crate name reflects that explicitly. If you're targeting another
HAL ecosystem you'd need to rewrite the bring-up layer; the ILI9488
register sequence and the FMC trick stay the same.

## Quick start

```rust,ignore
use embedded_graphics::{
    mono_font::{ascii::FONT_10X20, MonoTextStyle},
    pixelcolor::Rgb565,
    prelude::*,
    text::{Alignment, Text},
};
use riverdi_rva35hi_stm32u5_embassy::{Display, DisplayPins, PANEL_W, PANEL_H};

let p = embassy_stm32::init(Default::default());

let mut lcd = Display::new(DisplayPins {
    dc:  p.PE4,   // → FMC_A20 (= LCD_RS)
    rst: p.PE11,  // active-low reset
    bl:  p.PB8,   // backlight, active-high
    cs:  p.PD7,   // → FMC_NE1
});

lcd.clear(Rgb565::BLACK);
Text::with_alignment(
    "Hello",
    Point::new((PANEL_W / 2) as i32, (PANEL_H / 2) as i32),
    MonoTextStyle::new(&FONT_10X20, Rgb565::WHITE),
    Alignment::Center,
)
.draw(&mut lcd)
.unwrap();
```

If your HCLK is not 4 MHz, use the builder:

```rust,ignore
use riverdi_rva35hi_stm32u5_embassy::DisplayBuilder;

let lcd = DisplayBuilder::new(pins)
    .hclk_hz(160_000_000)
    .build();
```

To opt out of the driver's MPU_NS region 4 mapping (e.g. if your
project already uses region 4 for something else), pass `None`:

```rust,ignore
let lcd = DisplayBuilder::new(pins)
    .mpu_ns_region(None)  // you guarantee Device-nGnRnE on 0x6000_0000 yourself
    .build();
```

## Hardware

| Resource     | Pin / address                              | Notes                          |
|--------------|--------------------------------------------|--------------------------------|
| Data lines   | `PD14 PD15 PD0 PD1 PE7 PE8 PE9 PE10`       | `FMC_D0..D7` (AF12)            |
| RD / WR      | `PD4 PD5`                                  | `FMC_NOE / FMC_NWE`            |
| Chip select  | `PD7`                                      | `FMC_NE1`                      |
| DC select    | `PE4`                                      | `FMC_A20` — bit 20 of bus addr |
| Reset        | `PE11`                                     | GPIO output (see Gotcha 1)     |
| Backlight    | `PB8`                                      | GPIO output, active-high       |
| Cmd register | `0x6000_0000`                              | A20 = 0                        |
| Data register| `0x6020_0000`                              | A20 = 1                        |

## Three non-obvious gotchas on this shield

1. **`PE11` collides with `FMC_D8`.** The shield wires `PE11` to
   `LCD_RESET`. In the STM32 AF table, `PE11`'s AF12 is `FMC_D8`. If
   the FMC peripheral drives `PE11`, every bus transaction strobes
   `LCD_RESET` and the panel stays blank. The driver keeps `PE11` as
   a GPIO output held high after the reset pulse and uses
   `core::mem::forget` on the [`Output`] so embassy can never revert
   it.

2. **DC is wired to an FMC address bit, not a GPIO.** Riverdi's
   reference design connects the controller's RS pin to `A20` on the
   connector. We program `PE4` as `FMC_A20` (AF12) and toggle
   cmd/data by writing to two different bus addresses 2 MiB apart.
   No GPIO toggling, no race.

3. **`MWID` must be 16, not 8.** Even though the panel only reads
   `DB[7:0]`, the FMC bank needs 16-bit data mode so the address
   aliases line up cleanly (`A20` is the DC selector at 16-bit
   granularity). The HAL/TouchGFX workaround used by Riverdi's
   reference does the same. Bytes go in the low half of each 16-bit
   write; the upper byte is ignored by the panel.

## MPU mapping

The FMC bus lives at `0x6000_0000..0x6FFF_FFFF`. Under default
Normal-memory attributes the AHB matrix is free to merge adjacent
byte stores into a single wider transaction. On the LCD that drops
bytes from the parallel bus and the output collapses to uniform-
colour noise.

The driver therefore reprograms `MPU_NS` region 4 over the whole
bank as **Device-nGnRnE** (non-Gathering, non-Reordering, no Early
write ack) via `MAIR0[slot 3] = 0x00`. Region number is configurable
via [`DisplayBuilder::mpu_ns_region`]; pass `None` to skip the setup
entirely.

## Feature flags

| Feature        | Default | Effect                                                |
|----------------|---------|-------------------------------------------------------|
| `defmt`        | ✓       | Emit a couple of `defmt::info!` lines at bring-up.    |
| `embassy-task` | ✓       | Expose `display_task`, a minimal Embassy task entry.  |
| `stm32u575zi`  | ✓       | Enable `embassy-stm32/stm32u575zi`. Disable to target another U5 variant; you select the MCU feature on `embassy-stm32` yourself. |

To target a different U5 MCU:

```toml
[dependencies]
riverdi-rva35hi-stm32u5-embassy = { version = "0.1", default-features = false, features = ["defmt", "embassy-task"] }
embassy-stm32 = { version = "0.2", features = ["stm32u585zi", "unstable-pac", …] }
```

(Untested on variants other than U575ZI — the pins should be the
same on every U5 since they're peripheral-level, but the FMC base
addresses may differ. PRs welcome.)

## Reference

Init sequence and pin map cross-checked against
[`riverdi/riverdi-nucleo-3-50`](https://github.com/riverdi/riverdi-nucleo-3-50)
(`Core/Src/display.c`, `Core/Src/stm32h5xx_hal_msp.c`) and the
NUCLEO-U575ZI-Q user manual UM2861, Tables 17/18.

## License

Licensed under either of [Apache-2.0](LICENSE-APACHE) or
[MIT](LICENSE-MIT) at your option.
