package io.github.cepheus.app.ui

import android.content.ActivityNotFoundException
import android.content.Context
import android.net.Uri
import android.provider.OpenableColumns
import androidx.activity.compose.rememberLauncherForActivityResult
import androidx.activity.result.PickVisualMediaRequest
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.clickable
import androidx.compose.foundation.horizontalScroll
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material.icons.Icons
import androidx.compose.material.icons.filled.Close
import androidx.compose.material.icons.outlined.AttachFile
import androidx.compose.material.icons.outlined.Description
import androidx.compose.material.icons.outlined.ErrorOutline
import androidx.compose.material.icons.outlined.Image
import androidx.compose.material.icons.outlined.PhotoCamera
import androidx.compose.material3.CircularProgressIndicator
import androidx.compose.material3.DropdownMenu
import androidx.compose.material3.DropdownMenuItem
import androidx.compose.material3.Icon
import androidx.compose.material3.IconButton
import androidx.compose.material3.InputChip
import androidx.compose.material3.InputChipDefaults
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Text
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Modifier
import androidx.compose.ui.platform.LocalContext
import androidx.compose.ui.text.style.TextOverflow
import androidx.compose.ui.unit.dp
import androidx.core.content.FileProvider
import io.github.cepheus.app.Deck
import io.github.cepheus.app.upload.Attachment
import io.github.cepheus.app.upload.Source
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.launch
import java.io.File
import java.io.IOException
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale







@Composable
fun AttachMenu(
    enabled: Boolean,
    onAttach: (List<Uri>) -> Unit,
    onError: (String?) -> Unit,
    onCommands: (() -> Unit)? = null,
) {
    val context = LocalContext.current
    var open by remember { mutableStateOf(false) }
    // The camera may outlive this screen's state: keep where its photo goes.
    var cameraUri by rememberSaveable { mutableStateOf<String?>(null) }
    val photos = rememberLauncherForActivityResult(ActivityResultContracts.PickMultipleVisualMedia(Attachment.FILES_MAX)) {
        if (it.isNotEmpty()) onAttach(it)
    }
    val files = rememberLauncherForActivityResult(ActivityResultContracts.OpenMultipleDocuments()) {
        if (it.isNotEmpty()) onAttach(it)
    }
    val camera = rememberLauncherForActivityResult(ActivityResultContracts.TakePicture()) { saved ->
        val uri = cameraUri?.let(Uri::parse)
        cameraUri = null
        if (saved && uri != null) onAttach(listOf(uri))
    }
    Box {
        ControlCircleButton(DeckIcons.Plus, "Attach", onClick = { open = true }, enabled = enabled)
        DropdownMenu(
            expanded = open,
            onDismissRequest = { open = false },
            shape = RoundedCornerShape(18.dp),
            containerColor = deck.raised,
        ) {
            if (onCommands != null) {
                DropdownMenuItem(
                    text = { Text("Skills and commands") },
                    leadingIcon = { Icon(DeckIcons.Slash, contentDescription = null) },
                    onClick = {
                        open = false
                        onCommands()
                    },
                )
            }
            DropdownMenuItem(
                text = { Text("Photos") },
                leadingIcon = { Icon(Icons.Outlined.Image, contentDescription = null) },
                onClick = {
                    open = false
                    try {
                        photos.launch(PickVisualMediaRequest(ActivityResultContracts.PickVisualMedia.ImageAndVideo))
                    } catch (_: ActivityNotFoundException) {
                        onError("This phone has no photo picker.")
                    }
                },
            )
            DropdownMenuItem(
                text = { Text("Camera") },
                leadingIcon = { Icon(Icons.Outlined.PhotoCamera, contentDescription = null) },
                onClick = {
                    open = false
                    val uri = cameraFile(context)
                    cameraUri = uri.toString()
                    try {
                        camera.launch(uri)
                    } catch (_: ActivityNotFoundException) {
                        cameraUri = null
                        onError("This phone has no camera app.")
                    }
                },
            )
            DropdownMenuItem(
                text = { Text("Files") },
                leadingIcon = { Icon(Icons.Outlined.AttachFile, contentDescription = null) },
                onClick = {
                    open = false
                    try {
                        files.launch(arrayOf("*/*"))
                    } catch (_: ActivityNotFoundException) {
                        onError("This phone has no file picker.")
                    }
                },
            )
        }
    }
}

/** Where the camera saves a photo: the app's cache, shared with the camera app for this one picture. */
private fun cameraFile(context: Context): Uri {
    val dir = File(context.cacheDir, "camera").apply { mkdirs() }
    val dayAgo = System.currentTimeMillis() - 86_400_000L
    dir.listFiles()?.filter { it.lastModified() < dayAgo }?.forEach { it.delete() }
    val stamp = SimpleDateFormat("yyyyMMdd-HHmmss", Locale.ROOT).format(Date())
    return FileProvider.getUriForFile(context, "${context.packageName}.files", File(dir, "photo-$stamp.jpg"))
}

@Composable
fun AttachmentChips(list: List<Attachment>, onRemove: (Long) -> Unit, onRetry: (Long) -> Unit) {
    Row(
        Modifier.fillMaxWidth().horizontalScroll(rememberScrollState()).padding(start = 8.dp, end = 8.dp, top = 6.dp),
        horizontalArrangement = Arrangement.spacedBy(8.dp),
    ) {
        list.forEach { AttachmentChip(it, onRemove, onRetry) }
    }
}

@Composable
private fun AttachmentChip(a: Attachment, onRemove: (Long) -> Unit, onRetry: (Long) -> Unit) {
    val state = a.state
    val failed = state is Attachment.State.Failed
    val note = when (state) {
        is Attachment.State.Sending -> state.size?.takeIf { it > 0 }
            ?.let { "Sending ${(state.sent * 100 / it).coerceIn(0, 100)}%" }
            ?: "Sending ${Format.fileSize(state.sent)}"
        is Attachment.State.Ready -> Format.fileSize(state.size)
        is Attachment.State.Failed -> "Failed · tap to retry"
    }
    InputChip(
        selected = false,
        onClick = { if (failed) onRetry(a.key) },
        label = {
            Column(Modifier.padding(vertical = 4.dp)) {
                Text(
                    a.name,
                    maxLines = 1,
                    overflow = TextOverflow.Ellipsis,
                    style = MaterialTheme.typography.labelLarge,
                    modifier = Modifier.widthIn(max = 150.dp),
                )
                Text(
                    note,
                    style = MaterialTheme.typography.labelSmall,
                    color = if (failed) MaterialTheme.colorScheme.error else MaterialTheme.colorScheme.onSurfaceVariant,
                )
            }
        },
        avatar = {
            when (state) {
                is Attachment.State.Sending -> {
                    val done = state.size?.takeIf { it > 0 }?.let { state.sent.toFloat() / it }
                    if (done != null) {
                        CircularProgressIndicator(progress = { done }, modifier = Modifier.size(18.dp), strokeWidth = 2.dp)
                    } else {
                        CircularProgressIndicator(Modifier.size(18.dp), strokeWidth = 2.dp)
                    }
                }
                is Attachment.State.Ready -> Icon(
                    if (looksLikeImage(a.name)) Icons.Outlined.Image else Icons.Outlined.Description,
                    contentDescription = null,
                    modifier = Modifier.size(18.dp),
                )
                is Attachment.State.Failed -> Icon(
                    Icons.Outlined.ErrorOutline,
                    contentDescription = null,
                    tint = MaterialTheme.colorScheme.error,
                    modifier = Modifier.size(18.dp),
                )
            }
        },
        trailingIcon = {
            IconButton(onClick = { onRemove(a.key) }, modifier = Modifier.size(48.dp)) {
                Icon(Icons.Filled.Close, contentDescription = "Remove ${a.name}", modifier = Modifier.size(18.dp))
            }
        },
        shape = RoundedCornerShape(14.dp),
        colors = InputChipDefaults.inputChipColors(containerColor = deck.control),
        border = null,
        modifier = Modifier.heightIn(min = 48.dp),
    )
}

private fun looksLikeImage(name: String) =
    name.substringAfterLast('.', "").lowercase(Locale.ROOT) in setOf("jpg", "jpeg", "png", "gif", "webp", "heic", "heif", "bmp")
