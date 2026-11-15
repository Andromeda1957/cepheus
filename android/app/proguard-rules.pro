# The app has no reflective model: protocol messages are read with org.json.
# Manifest entry points are kept by the Android Gradle plugin.

# ML Kit's QR scanner resolves its services through Firebase component
# registrars at runtime; R8 optimising them made the chess app's scanner crash
# before the camera opened in release builds on some Android devices. Keep
# ML Kit and its component runtime intact here too.
-keep class com.google.mlkit.** { *; }
-keep class com.google.firebase.components.** { *; }
-keep class com.google.android.gms.internal.mlkit_code_scanner.** { *; }
