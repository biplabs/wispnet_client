plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
    id("dev.flutter.flutter-gradle-plugin")
}
val releaseVersion = providers.exec { workingDir(rootProject.file("../../..")); commandLine("sh", "scripts/build_identity.sh", "--version") }.standardOutput.asText.get().trim()
val sourceIdentity = providers.exec { workingDir(rootProject.file("../../..")); commandLine("sh", "scripts/build_identity.sh") }.standardOutput.asText.get().trim()
val nativeAbis = providers.gradleProperty("target-platform").orElse("android-arm64").get().split(",").map {
    when (it) { "android-arm64" -> "arm64-v8a"; "android-x64" -> "x86_64"; else -> error("Supported Android targets: android-arm64, android-x64") }
}
val stageWispnetNative = tasks.register<Sync>("stageWispnetNative") {
    val nativeRoot = file("src/main/jniLibs")
    from(nativeRoot) { nativeAbis.forEach { include("$it/libwispnet_android.so") } }
    into(layout.buildDirectory.dir("wispnet-jni"))
    doFirst {
        nativeAbis.forEach { require(nativeRoot.resolve("$it/libwispnet_android.so").isFile) { "Build the Rust library with scripts/build_android.sh first ($it)" } }
    }
}
// Keep upstream Java code while rebuilding its JNI helper for 16 KiB pages.
val termuxEmulatorArtifact by configurations.creating
val termuxEmulatorJava = tasks.register<Zip>("termuxEmulatorJava") {
    from({ zipTree(termuxEmulatorArtifact.singleFile) })
    exclude("jni/**")
    archiveFileName.set("terminal-emulator-java-0.118.0.aar")
    destinationDirectory.set(layout.buildDirectory.dir("termux-java"))
}
android {
    namespace = "com.biplabs.wispnet"
    compileSdk = 36
    ndkVersion = flutter.ndkVersion
    buildFeatures { compose = true }
    externalNativeBuild { cmake { path = file("src/main/cpp/CMakeLists.txt"); version = "3.22.1" } }
    sourceSets.getByName("main") {
        jniLibs.setSrcDirs(listOf(layout.buildDirectory.dir("wispnet-jni").get().asFile))
    }
    defaultConfig { applicationId = "app.wispnet.android"; minSdk = 34; targetSdk = 36; versionCode = flutter.versionCode; versionName = flutter.versionName; testInstrumentationRunner = "com.biplabs.wispnet.FilesTransferInstrumentation"; ndk { abiFilters.clear(); abiFilters += nativeAbis } }
    val uploadEnvironment = listOf("WISPNET_UPLOAD_KEYSTORE", "WISPNET_UPLOAD_STORE_PASSWORD", "WISPNET_UPLOAD_KEY_ALIAS", "WISPNET_UPLOAD_KEY_PASSWORD")
    val supplied = uploadEnvironment.map { providers.environmentVariable(it).orNull }
    require(supplied.all { it == null } || supplied.all { !it.isNullOrBlank() }) { "Supply all upload signing environment variables, or none for an unsigned build" }
    if (supplied.all { it != null }) {
        signingConfigs.create("upload") {
            storeFile = file(supplied[0]!!)
            storePassword = supplied[1]
            keyAlias = supplied[2]
            keyPassword = supplied[3]
        }
    }
    buildTypes {
        getByName("debug") { versionNameSuffix = sourceIdentity.removePrefix(releaseVersion) }
        getByName("release") {
            isDebuggable = false
            signingConfig = signingConfigs.findByName("upload")
        }
        create("peercheck") { initWith(getByName("debug")); applicationIdSuffix = ".peercheck"; signingConfig = signingConfigs.getByName("debug") }
    }
    packaging { jniLibs {
        useLegacyPackaging = false
        // Flutter and transitive AARs may otherwise add extra ABI payloads.
        (setOf("arm64-v8a", "armeabi-v7a", "x86", "x86_64") - nativeAbis.toSet()).forEach {
            excludes += "**/$it/**"
        }
    } }
    compileOptions { sourceCompatibility = JavaVersion.VERSION_17; targetCompatibility = JavaVersion.VERSION_17 }
}

kotlin { compilerOptions { jvmTarget.set(org.jetbrains.kotlin.gradle.dsl.JvmTarget.JVM_17) } }

// Android lifecycle annotations and native integration helpers.
dependencies {
    implementation("androidx.core:core:1.17.0")
    implementation("com.google.zxing:core:3.4.1")
    implementation(platform("androidx.compose:compose-bom:2024.09.03"))
    implementation("androidx.activity:activity-compose:1.9.2")
    implementation("androidx.compose.material3:material3")
    implementation("androidx.compose.ui:ui")
    // The older transitive 1.0.1 has a 4 KiB RELRO boundary despite 16 KiB LOADs.
    implementation("androidx.graphics:graphics-path:1.1.0")
    implementation("androidx.lifecycle:lifecycle-runtime-compose:2.8.6")
    implementation("com.termux.termux-app:terminal-view:0.118.0") { isTransitive = false }
    termuxEmulatorArtifact("com.termux.termux-app:terminal-emulator:0.118.0") { isTransitive = false }
    implementation(files(termuxEmulatorJava.flatMap { it.archiveFile }))
    testImplementation("junit:junit:4.13.2")
}

flutter { source = "../.." }

tasks.named("preBuild") { dependsOn(stageWispnetNative) }
