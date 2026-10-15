package dev.ohmyharness.mobile.ui.theme

import androidx.compose.material3.Typography
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.Font
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.sp
import dev.ohmyharness.mobile.R

val JetBrainsMono = FontFamily(
    Font(R.font.jetbrains_mono_regular, FontWeight.Normal),
    Font(R.font.jetbrains_mono_medium, FontWeight.Medium),
)

/** Code/path/commit/tool-output text; pair with `onSurfaceVariant` meta color. */
val CodeTextStyle = TextStyle(
    fontFamily = JetBrainsMono,
    fontSize = 13.sp,
    lineHeight = 19.sp,
)

private fun scaled(sp: Int, scale: Float) = (sp * scale).sp

/** Type scale at [scale] (1.0 = designed sizes); wired through the theme. */
fun ohmTypography(scale: Float = 1.0f): Typography = Typography(
    labelSmall = TextStyle(fontSize = scaled(11, scale), lineHeight = scaled(17, scale)),
    bodySmall = TextStyle(fontSize = scaled(12, scale), lineHeight = scaled(18, scale)),
    bodyMedium = TextStyle(fontSize = scaled(14, scale), lineHeight = scaled(21, scale)),
    bodyLarge = TextStyle(fontSize = scaled(15, scale), lineHeight = scaled(23, scale)),
    titleMedium = TextStyle(
        fontSize = scaled(16, scale), lineHeight = scaled(22, scale),
        fontWeight = FontWeight.Medium,
    ),
    headlineSmall = TextStyle(
        fontSize = scaled(18, scale), lineHeight = scaled(25, scale),
        fontWeight = FontWeight.SemiBold,
    ),
)

/** Default instance for call sites outside a composable theme. */
val OhmTypography = ohmTypography()
