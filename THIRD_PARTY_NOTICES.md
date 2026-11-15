# Third-party notices

Cepheus original code is distributed under the [MIT license](LICENSE).

- `src/input/parse.rs` contains code derived from crossterm, copyright
  2019 Timon, under MIT. Its full original notice is retained in that file.
- `android/gradlew`, `android/gradlew.bat`, and
  `android/gradle/wrapper/gradle-wrapper.jar` are Gradle wrapper components
  from the Gradle project, distributed under Apache-2.0. The scripts retain
  their original notices; the [Apache-2.0 license](LICENSES/Apache-2.0.txt)
  is included here.

Rust dependencies are pinned in `Cargo.lock`. Android dependencies are listed
in `android/gradle/libs.versions.toml` and the Gradle lockfiles. Each dependency
retains its own license; the MIT license for Cepheus does not replace those
licenses. Agent clients are installed separately and retain their own terms.
No agent credentials, personal configuration, conversations, or provider
artwork are bundled.
