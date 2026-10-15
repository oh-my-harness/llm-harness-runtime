package dev.ohmyharness.mobile.ui.theme

import androidx.compose.material3.ColorScheme
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.ui.graphics.Color

// Dark (primary developer palette)
private val DarkBackground = Color(0xFF0D1117)
private val DarkSurface = Color(0xFF161B22)
private val DarkSurfaceVariant = Color(0xFF21262D)
private val DarkPrimary = Color(0xFF4EC9B0)
private val DarkSecondary = Color(0xFFCE9178)
private val DarkError = Color(0xFFF85149)
private val DarkOnBackground = Color(0xFFE6EDF3)
private val DarkMeta = Color(0xFF8B949E)

// Light
private val LightBackground = Color(0xFFFFFFFF)
private val LightSurface = Color(0xFFF6F8FA)
private val LightSurfaceVariant = Color(0xFFEAEEF2)
private val LightPrimary = Color(0xFF0E7A6E)
private val LightSecondary = Color(0xFFB3552F)
private val LightError = Color(0xFFC73E3E)
private val LightOnBackground = Color(0xFF1F2328)
private val LightMeta = Color(0xFF656D76)

fun ohmDarkColorScheme(): ColorScheme = darkColorScheme(
    primary = DarkPrimary,
    onPrimary = Color(0xFF0D1117),
    secondary = DarkSecondary,
    error = DarkError,
    background = DarkBackground,
    onBackground = DarkOnBackground,
    surface = DarkSurface,
    onSurface = DarkOnBackground,
    surfaceVariant = DarkSurfaceVariant,
    onSurfaceVariant = DarkMeta,
    outline = DarkMeta,
)

fun ohmLightColorScheme(): ColorScheme = lightColorScheme(
    primary = LightPrimary,
    onPrimary = Color.White,
    secondary = LightSecondary,
    error = LightError,
    background = LightBackground,
    onBackground = LightOnBackground,
    surface = LightSurface,
    onSurface = LightOnBackground,
    surfaceVariant = LightSurfaceVariant,
    onSurfaceVariant = LightMeta,
    outline = LightMeta,
)
