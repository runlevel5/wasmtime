use crate::cdsl::isa::TargetIsa;
use crate::cdsl::settings::SettingGroupBuilder;

pub(crate) fn define() -> TargetIsa {
    let mut settings = SettingGroupBuilder::new("ppc64");

    // The baseline for this backend is POWER8 (ISA 2.07): the first
    // little-endian-capable generation, and the baseline assumed by
    // distributions shipping ppc64le. VSX and VMX are therefore always
    // available and do not get their own flags. Only later facilities are
    // listed here.

    // POWER9 (ISA 3.0) facilities.
    let has_isa_3_0 = settings.add_bool(
        "has_isa_3_0",
        "Has ISA 3.0 (POWER9) instruction support.",
        "Enables the modulo instructions (modsd/modud), setb, darn, and the \
         ISA 3.0 scalar floating-point conversions.",
        false,
    );

    // POWER10 (ISA 3.1) facilities.
    let has_isa_3_1 = settings.add_bool(
        "has_isa_3_1",
        "Has ISA 3.1 (POWER10) instruction support.",
        "Enables prefixed instructions, most importantly PC-relative \
         addressing via paddi/pld, which removes the need to materialize \
         addresses through immediate sequences or a TOC.",
        false,
    );

    // Processor presets. These are cumulative: each generation implies the
    // facilities of the one before it.
    settings.add_preset("power9", "IBM POWER9 processor.", preset!(has_isa_3_0));
    settings.add_preset(
        "power10",
        "IBM POWER10 processor.",
        preset!(has_isa_3_0 && has_isa_3_1),
    );

    TargetIsa::new("ppc64", settings.build())
}
