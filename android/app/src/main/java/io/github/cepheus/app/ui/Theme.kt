package io.github.cepheus.app.ui

import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.material3.ButtonColors
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.LocalContentColor
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Typography
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.Immutable
import androidx.compose.runtime.staticCompositionLocalOf
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.sp
import io.github.cepheus.app.protocol.Status

/**
 * Semantic colors shared by the app's light and dark themes. Blue identifies
 * actions and links, gold marks sessions needing attention, and green marks
 * finished work. User messages use a slate-blue surface.
 */
@Immutable
data class DeckColors(
    /** The page behind everything. */
    val page: Color,
    /** Raised surfaces: the composer, cards, sheets and floating buttons. */
    val raised: Color,
    /** Your own messages. */
    val bubble: Color,
    /** Round controls and pills inside raised surfaces. */
    val control: Color,
    /** A selected filter chip. */
    val selected: Color,
    /** Hairlines around raised surfaces; shadows carry them in light mode. */
    val border: Color,
    /** The blue fill of the send button and the new-session button. */
    val accent: Color,
    val onAccent: Color,
    val text: Color,
    val textSecondary: Color,
    val textTertiary: Color,
    /** Code blocks and terminal output. */
    val code: Color,
    /** Whether shadows show: dark mode leans on hairlines instead. */
    val shadows: Boolean,
)

internal val LightDeck = DeckColors(
    page = Color(0xFFF6F7FA),
    raised = Color(0xFFFFFFFF),
    bubble = Color(0xFFE2EBF6),
    control = Color(0xFFEDF0F4),
    selected = Color(0xFFDAEAFA),
    border = Color(0x1F15202C),
    accent = Color(0xFF2A6EC2),
    onAccent = Color(0xFFFFFFFF),
    text = Color(0xFF15202C),
    textSecondary = Color(0xFF465463),
    textTertiary = Color(0xFF5E6B79),
    code = Color(0xFFF0F3F7),
    shadows = true,
)

internal val DarkDeck = DeckColors(
    page = Color(0xFF121315),
    raised = Color(0xFF1B1D20),
    bubble = Color(0xFF22303F),
    control = Color(0xFF2F3338),
    selected = Color(0xFF34445C),
    border = Color(0x1AFFFFFF),
    accent = Color(0xFF2A6EC2),
    onAccent = Color(0xFFFFFFFF),
    text = Color(0xFFECEEF1),
    textSecondary = Color(0xFFB4BAC1),
    // A step above the app's #8B929A, so it clears 4.5:1 on the grey controls too.
    textTertiary = Color(0xFF969DA5),
    code = Color(0xFF1B1D20),
    shadows = false,
)


internal val LightLink = Color(0xFF075EAA)
internal val DarkLink = Color(0xFF62ABF6)

private val Light = lightColorScheme(
    primary = LightLink,
    onPrimary = Color(0xFFFFFFFF),
    primaryContainer = LightDeck.bubble,
    onPrimaryContainer = LightDeck.text,
    secondary = LightDeck.textSecondary,
    onSecondary = Color(0xFFFFFFFF),
    secondaryContainer = LightDeck.selected,
    onSecondaryContainer = LightDeck.text,
    tertiary = Color(0xFF856128),
    onTertiary = Color(0xFFFFFFFF),
    background = LightDeck.page,
    onBackground = LightDeck.text,
    surface = LightDeck.page,
    onSurface = LightDeck.text,
    surfaceVariant = LightDeck.control,
    onSurfaceVariant = LightDeck.textSecondary,
    surfaceTint = Color.Transparent,
    surfaceBright = Color(0xFFFFFFFF),
    surfaceDim = Color(0xFFE3E7EC),
    surfaceContainerLowest = Color(0xFFFFFFFF),
    surfaceContainerLow = Color(0xFFFFFFFF),
    surfaceContainer = Color(0xFFFFFFFF),
    surfaceContainerHigh = Color(0xFFFFFFFF),
    surfaceContainerHighest = LightDeck.control,
    outline = Color(0xFF697888),
    outlineVariant = Color(0xFFDCE2E9),
    error = Color(0xFFB23F3F),
    onError = Color(0xFFFFFFFF),
    errorContainer = Color(0xFFFBE3E3),
    onErrorContainer = Color(0xFF4A1414),
)

private val Dark = darkColorScheme(
    primary = DarkLink,
    onPrimary = DarkDeck.page,
    primaryContainer = DarkDeck.control,
    onPrimaryContainer = DarkDeck.text,
    secondary = DarkDeck.textSecondary,
    onSecondary = DarkDeck.page,
    secondaryContainer = DarkDeck.selected,
    onSecondaryContainer = DarkDeck.text,
    tertiary = Color(0xFFD0AC72),
    onTertiary = DarkDeck.page,
    background = DarkDeck.page,
    onBackground = DarkDeck.text,
    surface = DarkDeck.page,
    onSurface = DarkDeck.text,
    surfaceVariant = DarkDeck.raised,
    onSurfaceVariant = DarkDeck.textSecondary,
    surfaceTint = Color.Transparent,
    surfaceBright = DarkDeck.control,
    surfaceDim = Color(0xFF0D0E10),
    surfaceContainerLowest = Color(0xFF0D0E10),
    surfaceContainerLow = DarkDeck.raised,
    surfaceContainer = DarkDeck.raised,
    surfaceContainerHigh = DarkDeck.raised,
    surfaceContainerHighest = DarkDeck.control,
    outline = DarkDeck.textTertiary,
    outlineVariant = DarkDeck.control,
    error = Color(0xFFEE8A8A),
    onError = DarkDeck.page,
    errorContainer = Color(0xFFB23F3F),
    onErrorContainer = Color(0xFFFFFFFF),
)

/** The assistant's prose is set in a serif, as in the Claude app. */
val ProseFamily = FontFamily.Serif

/**
 * Claude sets interface text in a sans at 16 sp with generous leading and
 * semibold titles, and large headings in its serif.
 */
private fun deckTypography(): Typography {
    val base = Typography()
    val sans = FontFamily.Default
    return base.copy(
        displaySmall = TextStyle(fontFamily = ProseFamily, fontSize = 34.sp, lineHeight = 40.sp),
        headlineMedium = TextStyle(fontFamily = ProseFamily, fontSize = 30.sp, lineHeight = 36.sp),
        headlineSmall = TextStyle(fontFamily = ProseFamily, fontSize = 26.sp, lineHeight = 32.sp),
        titleLarge = TextStyle(fontFamily = sans, fontWeight = FontWeight.SemiBold, fontSize = 20.sp, lineHeight = 26.sp),
        titleMedium = TextStyle(fontFamily = sans, fontWeight = FontWeight.SemiBold, fontSize = 16.sp, lineHeight = 22.sp),
        titleSmall = TextStyle(fontFamily = sans, fontWeight = FontWeight.SemiBold, fontSize = 14.sp, lineHeight = 20.sp),
        bodyLarge = TextStyle(fontFamily = sans, fontSize = 16.sp, lineHeight = 24.sp),
        bodyMedium = TextStyle(fontFamily = sans, fontSize = 14.sp, lineHeight = 20.sp),
        bodySmall = TextStyle(fontFamily = sans, fontSize = 13.sp, lineHeight = 18.sp),
        labelLarge = TextStyle(fontFamily = sans, fontWeight = FontWeight.Medium, fontSize = 15.sp, lineHeight = 20.sp),
        labelMedium = TextStyle(fontFamily = sans, fontWeight = FontWeight.Medium, fontSize = 13.sp, lineHeight = 18.sp),
        labelSmall = TextStyle(fontFamily = sans, fontWeight = FontWeight.Medium, fontSize = 12.sp, lineHeight = 16.sp),
    )
}

/** Whether the app is dark: Settings' Theme, or the phone's when that follows the system. */
val LocalDarkTheme = staticCompositionLocalOf { false }

/** The layout's roles beyond Material's scheme. */
val LocalDeckColors = staticCompositionLocalOf { LightDeck }

/** Shorthand for the current [DeckColors]. */
val deck: DeckColors
    @Composable get() = LocalDeckColors.current

/**
 * A filled button in the blue fill that sends, starts and confirms. Material's
 * default would fill it with the scheme's primary, the lighter link blue.
 */
@Composable
fun accentButtonColors(): ButtonColors = ButtonDefaults.buttonColors(
    containerColor = deck.accent,
    contentColor = deck.onAccent,
    disabledContainerColor = deck.control,
    disabledContentColor = deck.textTertiary,
)

@Composable
fun CepheusTheme(dark: Boolean = isSystemInDarkTheme(), content: @Composable () -> Unit) {
    val colors = if (dark) DarkDeck else LightDeck
    CompositionLocalProvider(
        LocalDarkTheme provides dark,
        LocalDeckColors provides colors,
    ) {
        // The screens draw their page as a plain background, not a Surface,
        // so text without its own colour takes the deck's from here; without
        // it the replies stayed black on the dark page (emulator check,
        // 2026-11-01).
        MaterialTheme(colorScheme = if (dark) Dark else Light, typography = deckTypography()) {
            CompositionLocalProvider(LocalContentColor provides colors.text, content = content)
        }
    }
}

@Composable
fun statusColor(status: Status): Color {
    val dark = LocalDarkTheme.current
    return when (status) {
        Status.WORKING -> if (dark) DarkLink else LightDeck.accent
        Status.STARTING -> if (dark) Color(0xFFA9C8EE) else Color(0xFF6C93C6)
        Status.NEEDS_YOU -> if (dark) Gold else LightGold
        Status.FINISHED -> if (dark) Green else LightGreen
        Status.IDLE -> MaterialTheme.colorScheme.outline
        Status.ENDED, Status.UNKNOWN -> MaterialTheme.colorScheme.outline.copy(alpha = 0.7f)
    }
}


internal val Gold = Color(0xFFD0AC72)
internal val LightGold = Color(0xFF856128)


internal val Green = Color(0xFF7FC46A)
internal val LightGreen = Color(0xFF346D24)





internal fun codeColor(kind: CodeTokenKind, dark: Boolean): Color = when (kind) {
    CodeTokenKind.KEYWORD -> if (dark) Color(0xFFB18CF0) else Color(0xFF712DE4)
    CodeTokenKind.STRING -> if (dark) Green else LightGreen
    CodeTokenKind.NUMBER -> if (dark) Gold else LightGold
    CodeTokenKind.COMMENT -> if (dark) DarkDeck.textSecondary else LightDeck.textSecondary
}
