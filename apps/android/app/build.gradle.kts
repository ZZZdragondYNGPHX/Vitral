import java.io.File
import java.security.MessageDigest

plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
}

val bundledModels = tasks.register<Sync>("bundleModels") {
    from(rootProject.file("../../assets")) {
        include("rime/cn_dicts/8105.dict.yaml", "rime/cn_dicts/41448.dict.yaml",
            "rime/cn_dicts/base.dict.yaml", "rime/cn_dicts/ext.dict.yaml", "rime/cn_dicts/others.dict.yaml",
            "rime/en_dicts/en.dict.yaml", "rime/en_dicts/en_ext.dict.yaml",
            "rime/en_dicts/cn_en.txt", "rime/en_dicts/cn_en_flypy.txt",
            "rime/radical_pinyin.dict.yaml",
            "rime/LICENSE", "opencc/**")
        exclude("**/.*.sha256")
    }
    from(rootProject.file("../../licenses")) { into("licenses") }
    from(rootProject.files("../../LICENSE-MIT", "../../LICENSE-APACHE")) { into("licenses") }
    from(rootProject.files("../../scripts/assets-manifest.json", "../../scripts/models-catalog.json"))
    into(layout.buildDirectory.dir("generated/assets/models"))
    doLast {
        val digest = MessageDigest.getInstance("SHA-256")
        val root = destinationDir
        root.walkTopDown()
            .filter { it.isFile && it.name != "fingerprint.txt" }
            .sortedBy { it.relativeTo(root).invariantSeparatorsPath }
            .forEach { file ->
                digest.update(file.relativeTo(root).invariantSeparatorsPath.toByteArray(Charsets.UTF_8))
                digest.update(0)
                file.inputStream().use { input ->
                    val buf = ByteArray(16384)
                    while (true) {
                        val n = input.read(buf)
                        if (n <= 0) break
                        digest.update(buf, 0, n)
                    }
                }
                digest.update(0)
            }
        val hex = digest.digest().take(8).joinToString("") { b -> "%02x".format(b) }
        File(root, "fingerprint.txt").writeText(hex)
    }
}

android {
    namespace = "dev.vitral.ime"
    compileSdk = 35
    ndkVersion = "27.3.13750724"
    defaultConfig {
        applicationId = "dev.vitral.ime"
        minSdk = 29
        targetSdk = 35
        versionCode = 21
        versionName = "0.4.18"
        ndk { abiFilters += listOf("arm64-v8a", "armeabi-v7a", "x86_64") }
    }
    buildFeatures { compose = true }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions { jvmTarget = "17" }
    sourceSets["main"].assets.srcDir(layout.buildDirectory.dir("generated/assets"))
}
tasks.named("preBuild") { dependsOn(bundledModels) }

dependencies {
    implementation(platform("androidx.compose:compose-bom:2025.05.01"))
    implementation("androidx.activity:activity-compose:1.10.1")
    implementation("androidx.autofill:autofill:1.1.0")
    implementation("androidx.compose.material3:material3")
    implementation("com.microsoft.onnxruntime:onnxruntime-android:1.23.2")
    testImplementation("junit:junit:4.13.2")
}
