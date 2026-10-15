package dev.ohmyharness.mobile.ui.theme

import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.runtime.Composable

/**
 * Developer-tool theme with a global type-scale multiplier from Settings.
 * Fixed ohm palettes (no Material You dynamic color); light/dark follow the
 * system.
 */
@Composable
fun OhmMobileTheme(fontScale: Float = 1.0f, content: @Composable () -> Unit) {
    val dark = isSystemInDarkTheme()
    MaterialTheme(
        colorScheme = if (dark) ohmDarkColorScheme() else ohmLightColorScheme(),
        typography = ohmTypography(scale = fontScale),
        content = content,
    )
}
