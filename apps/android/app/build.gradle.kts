plugins {
    id("com.android.application")
    id("org.jetbrains.kotlin.android")
    id("org.jetbrains.kotlin.plugin.compose")
}

val bundledModels = tasks.register<Sync>("bundleModels") {
    from(rootProject.file("../../assets")) {
        include("rime/cn_dicts/8105.dict.yaml", "rime/cn_dicts/41448.dict.yaml",
            "rime/cn_dicts/base.dict.yaml", "rime/cn_dicts/ext.dict.yaml", "rime/cn_dicts/others.dict.yaml",
            "rime/LICENSE", "rime/*.gram", "handwriting/hw_model.int8.onnx", "handwriting/hw_model.classes.json",
            "handwriting/cjkvi_ids.txt", "reranker/model_int8.onnx", "reranker/vocab.txt",
            "voice/sensevoice-small/model.int8.onnx", "voice/sensevoice-small/tokens.txt", "voice/sensevoice-small/LICENSE")
    }
    from(rootProject.file("../../licenses")) { into("licenses") }
    from(rootProject.files("../../LICENSE-MIT", "../../LICENSE-APACHE")) { into("licenses") }
    from(rootProject.file("../../scripts/assets-manifest.json"))
    into(layout.buildDirectory.dir("generated/assets/models"))
}

android {
    namespace = "dev.vitral.ime"
    compileSdk = 35
    ndkVersion = "27.3.13750724"
    defaultConfig {
        applicationId = "dev.vitral.ime"
        minSdk = 29
        targetSdk = 35
        versionCode = 2
        versionName = "0.2.0"
        ndk { abiFilters += listOf("arm64-v8a", "armeabi-v7a", "x86_64") }
    }
    buildFeatures { compose = true }
    compileOptions {
        sourceCompatibility = JavaVersion.VERSION_17
        targetCompatibility = JavaVersion.VERSION_17
    }
    kotlinOptions { jvmTarget = "17" }
    sourceSets["main"].assets.srcDir(layout.buildDirectory.dir("generated/assets"))
    androidResources { noCompress += listOf("onnx", "gram") }
}
tasks.named("preBuild") { dependsOn(bundledModels) }

dependencies {
    implementation(platform("androidx.compose:compose-bom:2025.05.01"))
    implementation("androidx.activity:activity-compose:1.10.1")
    implementation("androidx.compose.material3:material3")
    implementation("com.microsoft.onnxruntime:onnxruntime-android:1.23.2")
    testImplementation("junit:junit:4.13.2")
}
