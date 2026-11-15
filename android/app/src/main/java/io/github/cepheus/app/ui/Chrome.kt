package io.github.cepheus.app.ui

import androidx.compose.animation.core.animateFloatAsState
import androidx.compose.animation.core.tween
import androidx.compose.foundation.interaction.MutableInteractionSource
import androidx.compose.foundation.interaction.collectIsPressedAsState
import androidx.compose.runtime.getValue
import androidx.compose.runtime.remember
import androidx.compose.ui.graphics.graphicsLayer
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.background
import androidx.compose.foundation.clickable
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.RowScope
import androidx.compose.foundation.layout.height
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Surface
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.draw.clip
import androidx.compose.ui.draw.drawBehind
import androidx.compose.ui.geometry.Offset
import androidx.compose.ui.geometry.Size
import androidx.compose.ui.graphics.Brush
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.graphics.PathFillType
import androidx.compose.ui.graphics.SolidColor
import androidx.compose.ui.graphics.StrokeCap
import androidx.compose.ui.graphics.StrokeJoin
import androidx.compose.ui.graphics.vector.ImageVector
import androidx.compose.ui.graphics.vector.path
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.role
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.Dp
import androidx.compose.ui.unit.dp

/**
 * Line icons drawn as Claude's app draws its own: thin round-capped strokes on
 * a 24-unit grid. Material's filled set read heavier than the rest of the page.
 */
object DeckIcons {
    private fun stroked(name: String, width: Float = 1.7f, block: androidx.compose.ui.graphics.vector.PathBuilder.() -> Unit) =
        ImageVector.Builder(name, 24.dp, 24.dp, 24f, 24f).apply {
            path(
                stroke = SolidColor(Color.Black),
                strokeLineWidth = width,
                strokeLineCap = StrokeCap.Round,
                strokeLineJoin = StrokeJoin.Round,
                pathBuilder = block,
            )
        }.build()

    /** Three left-aligned lines, each shorter: the drawer. */
    val Menu: ImageVector = stroked("Menu") {
        moveTo(4f, 6.5f); lineTo(20f, 6.5f)
        moveTo(4f, 12f); lineTo(15.5f, 12f)
        moveTo(4f, 17.5f); lineTo(11f, 17.5f)
    }

    val Plus: ImageVector = stroked("Plus") {
        moveTo(12f, 5f); lineTo(12f, 19f)
        moveTo(5f, 12f); lineTo(19f, 12f)
    }

    val ArrowUp: ImageVector = stroked("ArrowUp", 2.2f) {
        moveTo(12f, 19f); lineTo(12f, 5.5f)
        moveTo(6f, 11.5f); lineTo(12f, 5.5f); lineTo(18f, 11.5f)
    }

    val ArrowDown: ImageVector = stroked("ArrowDown", 2f) {
        moveTo(12f, 5f); lineTo(12f, 18.5f)
        moveTo(6f, 12.5f); lineTo(12f, 18.5f); lineTo(18f, 12.5f)
    }

    val Back: ImageVector = stroked("Back", 1.9f) {
        moveTo(19f, 12f); lineTo(5.5f, 12f)
        moveTo(11.5f, 6f); lineTo(5.5f, 12f); lineTo(11.5f, 18f)
    }

    val ChevronDown: ImageVector = stroked("ChevronDown", 1.8f) {
        moveTo(7f, 10f); lineTo(12f, 15f); lineTo(17f, 10f)
    }

    val Check: ImageVector = stroked("Check", 2f) {
        moveTo(5f, 12.5f); lineTo(10f, 17.5f); lineTo(19f, 7.5f)
    }

    val Close: ImageVector = stroked("Close", 1.8f) {
        moveTo(6.5f, 6.5f); lineTo(17.5f, 17.5f)
        moveTo(17.5f, 6.5f); lineTo(6.5f, 17.5f)
    }

    val Mic: ImageVector = stroked("Mic") {
        moveTo(12f, 3.5f)
        curveTo(10.3f, 3.5f, 9f, 4.8f, 9f, 6.5f)
        lineTo(9f, 11.5f)
        curveTo(9f, 13.2f, 10.3f, 14.5f, 12f, 14.5f)
        curveTo(13.7f, 14.5f, 15f, 13.2f, 15f, 11.5f)
        lineTo(15f, 6.5f)
        curveTo(15f, 4.8f, 13.7f, 3.5f, 12f, 3.5f)
        close()
        moveTo(6f, 11f)
        curveTo(6f, 14.3f, 8.7f, 17f, 12f, 17f)
        curveTo(15.3f, 17f, 18f, 14.3f, 18f, 11f)
        moveTo(12f, 17f); lineTo(12f, 20.5f)
    }

    /** Horizontal dots: more actions. */
    val More: ImageVector = ImageVector.Builder("More", 24.dp, 24.dp, 24f, 24f).apply {
        path(fill = SolidColor(Color.Black)) {
            for (x in listOf(5.5f, 12f, 18.5f)) {
                moveTo(x + 1.7f, 12f)
                arcTo(1.7f, 1.7f, 0f, true, true, x - 1.7f, 12f)
                arcTo(1.7f, 1.7f, 0f, true, true, x + 1.7f, 12f)
                close()
            }
        }
    }.build()

    /** A filled speech bubble with a plus cut through it: start something new. */
    val NewChat: ImageVector = ImageVector.Builder("NewChat", 24.dp, 24.dp, 24f, 24f).apply {
        path(fill = SolidColor(Color.Black), pathFillType = PathFillType.EvenOdd) {
            // The bubble: a circle with a tail at its lower left.
            moveTo(12f, 3f)
            curveTo(16.97f, 3f, 21f, 6.81f, 21f, 11.5f)
            curveTo(21f, 16.19f, 16.97f, 20f, 12f, 20f)
            curveTo(10.85f, 20f, 9.75f, 19.8f, 8.74f, 19.43f)
            lineTo(4.2f, 21f)
            lineTo(5.2f, 17.1f)
            curveTo(3.83f, 15.6f, 3f, 13.64f, 3f, 11.5f)
            curveTo(3f, 6.81f, 7.03f, 3f, 12f, 3f)
            close()
            // The plus, cut out.
            moveTo(11.1f, 7.6f); lineTo(12.9f, 7.6f); lineTo(12.9f, 10.6f); lineTo(15.9f, 10.6f)
            lineTo(15.9f, 12.4f); lineTo(12.9f, 12.4f); lineTo(12.9f, 15.4f); lineTo(11.1f, 15.4f)
            lineTo(11.1f, 12.4f); lineTo(8.1f, 12.4f); lineTo(8.1f, 10.6f); lineTo(11.1f, 10.6f)
            close()
        }
    }.build()

    val Stop: ImageVector = ImageVector.Builder("Stop", 24.dp, 24.dp, 24f, 24f).apply {
        path(fill = SolidColor(Color.Black)) {
            moveTo(9f, 7.5f); lineTo(15f, 7.5f)
            curveTo(15.83f, 7.5f, 16.5f, 8.17f, 16.5f, 9f)
            lineTo(16.5f, 15f)
            curveTo(16.5f, 15.83f, 15.83f, 16.5f, 15f, 16.5f)
            lineTo(9f, 16.5f)
            curveTo(8.17f, 16.5f, 7.5f, 15.83f, 7.5f, 15f)
            lineTo(7.5f, 9f)
            curveTo(7.5f, 8.17f, 8.17f, 7.5f, 9f, 7.5f)
            close()
        }
    }.build()

    /** A clock face: earlier conversations. */
    val History: ImageVector = stroked("History") {
        moveTo(4.5f, 12f)
        arcTo(7.5f, 7.5f, 0f, true, true, 12f, 19.5f)
        moveTo(4.5f, 12f); lineTo(4.5f, 7.5f)
        moveTo(4.5f, 12f); lineTo(8.5f, 12f)
        moveTo(12f, 8f); lineTo(12f, 12f); lineTo(15f, 14f)
    }

    /** Two chevrons and a bar: a terminal. */
    val Terminal: ImageVector = stroked("Terminal") {
        moveTo(5f, 7.5f); lineTo(9.5f, 12f); lineTo(5f, 16.5f)
        moveTo(12f, 17f); lineTo(19f, 17f)
    }

    val Slash: ImageVector = stroked("Slash", 1.8f) {
        moveTo(15f, 4.5f); lineTo(9f, 19.5f)
    }

    val Copy: ImageVector = stroked("Copy", 1.6f) {
        moveTo(9f, 9f); lineTo(18f, 9f); lineTo(18f, 19f); lineTo(9f, 19f); close()
        moveTo(6f, 15f); lineTo(6f, 5f); lineTo(15f, 5f)
    }
}

/** A raised round or pill surface: white with a soft shadow, or a hairline in dark mode. */
@Composable
fun Floating(
    modifier: Modifier = Modifier,
    shape: androidx.compose.ui.graphics.Shape = CircleShape,
    color: Color = deck.raised,
    content: @Composable () -> Unit,
) {
    val colors = deck
    Surface(
        shape = shape,
        color = color,
        shadowElevation = if (colors.shadows) 3.dp else 0.dp,
        border = if (colors.shadows) null else BorderStroke(1.dp, colors.border),
        modifier = modifier,
        content = content,
    )
}

/** Every control keeps a 48 dp touch target, however small it draws. */
private val Touch = 48.dp

/**
 * A touch target of [Touch] around a smaller drawn control, so the Claude
 * app's 40 dp circles stay easy to hit (the chat check measures 48 dp).
 */
@Composable
private fun Target(
    description: String,
    onClick: () -> Unit,
    enabled: Boolean,
    modifier: Modifier = Modifier,
    width: Dp? = Touch,
    content: @Composable () -> Unit,
) {
    val interaction = remember { MutableInteractionSource() }
    val pressed by interaction.collectIsPressedAsState()
    val scale by animateFloatAsState(if (pressed && enabled) 0.94f else 1f, tween(100), label = "control press")
    Box(
        modifier
            .then(if (width != null) Modifier.size(width, Touch) else Modifier.height(Touch))
            .graphicsLayer { scaleX = scale; scaleY = scale }
            .clip(RoundedCornerShape(50))
            .clickable(interactionSource = interaction, indication = androidx.compose.foundation.LocalIndication.current, enabled = enabled, role = Role.Button, onClick = onClick)
            .semantics { contentDescription = description },
        contentAlignment = Alignment.Center,
    ) { content() }
}

/** A round floating button, as the Claude app's drawer button. */
@Composable
fun FloatingIconButton(
    icon: ImageVector,
    description: String,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
    enabled: Boolean = true,
) {
    Target(description, onClick, enabled, modifier) {
        Floating(Modifier.size(44.dp)) {
            Box(contentAlignment = Alignment.Center) {
                Icon(icon, contentDescription = null, tint = deck.text, modifier = Modifier.size(22.dp))
            }
        }
    }
}

/** Several buttons in one floating pill, as the Claude app's new-chat and overflow pair. */
@Composable
fun FloatingPill(modifier: Modifier = Modifier, content: @Composable RowScope.() -> Unit) {
    Floating(modifier, shape = RoundedCornerShape(50)) {
        Row(Modifier.height(44.dp).padding(horizontal = 2.dp), verticalAlignment = Alignment.CenterVertically, content = content)
    }
}

/** One button inside a [FloatingPill]. */
@Composable
fun PillIconButton(icon: ImageVector, description: String, onClick: () -> Unit, enabled: Boolean = true) {
    IconButton(onClick = onClick, enabled = enabled, modifier = Modifier.size(44.dp)) {
        Icon(icon, contentDescription = description, tint = deck.text, modifier = Modifier.size(22.dp))
    }
}

/** The blue round button that starts something: send, or a new session. */
@Composable
fun AccentCircleButton(
    icon: ImageVector,
    description: String,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
    enabled: Boolean = true,
    size: Dp = 40.dp,
    iconSize: Dp = 22.dp,
) {
    val colors = deck
    Target(description, onClick, enabled, modifier, width = maxOf(size, Touch)) {
        Box(
            Modifier.size(size).background(if (enabled) colors.accent else colors.accent.copy(alpha = 0.4f), CircleShape),
            contentAlignment = Alignment.Center,
        ) {
            Icon(icon, contentDescription = null, tint = colors.onAccent, modifier = Modifier.size(iconSize))
        }
    }
}

/** A grey round control inside a raised surface: attach, dictate. */
@Composable
fun ControlCircleButton(
    icon: ImageVector,
    description: String,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
    enabled: Boolean = true,
    size: Dp = 40.dp,
) {
    val colors = deck
    Target(description, onClick, enabled, modifier) {
        Box(Modifier.size(size).background(colors.control, CircleShape), contentAlignment = Alignment.Center) {
            Icon(
                icon,
                contentDescription = null,
                tint = if (enabled) colors.text else colors.textTertiary,
                modifier = Modifier.size(21.dp),
            )
        }
    }
}

/** A grey pill holding a label, as the composer's model pill. */
@Composable
fun ControlPill(
    label: String,
    description: String,
    onClick: () -> Unit,
    modifier: Modifier = Modifier,
    enabled: Boolean = true,
    trailing: ImageVector? = null,
) {
    val colors = deck
    Target(description, onClick, enabled, modifier, width = null) {
        Row(
            Modifier.height(40.dp).background(colors.control, RoundedCornerShape(50)).padding(horizontal = 14.dp),
            verticalAlignment = Alignment.CenterVertically,
        ) {
            Text(
                label,
                style = MaterialTheme.typography.labelLarge,
                color = colors.text,
                maxLines = 1,
                overflow = TextOverflow.Ellipsis,
                modifier = Modifier.weight(1f, fill = false),
            )
            if (trailing != null) {
                Icon(trailing, contentDescription = null, tint = colors.text, modifier = Modifier.padding(start = 4.dp).size(16.dp))
            }
        }
    }
}

/**
 * The page behind the floating controls at the top of a screen: solid under
 * them, then fading out over [fade] below. A fade across the header itself
 * let scrolled replies show through the chat's title and status (emulator
 * capture, 2026-11-01).
 */
fun Modifier.headerBand(page: Color, fade: Dp = 16.dp): Modifier = drawBehind {
    drawRect(page)
    val reach = fade.toPx()
    drawRect(
        Brush.verticalGradient(listOf(page, page.copy(alpha = 0f)), startY = size.height, endY = size.height + reach),
        topLeft = Offset(0f, size.height),
        size = Size(size.width, reach),
    )
}

/** A small coloured dot for a session's state. */
@Composable
fun StateDot(color: Color, modifier: Modifier = Modifier, size: Dp = 8.dp) {
    Box(modifier.size(size).background(color, CircleShape))
}
