// swift-tools-version:6.0
import PackageDescription

// Library and tests only; upstream CLI tools and build-info generator are omitted.
// See UPSTREAM.json for the exact source revision and local changes.
let package = Package(
    name: "SwiftTerm",
    platforms: [.macOS(.v14)],
    products: [.library(name: "SwiftTerm", targets: ["SwiftTerm"])],
    targets: [
        .target(
            name: "SwiftTerm",
            exclude: ["Mac/README.md"],
            resources: [.process("Apple/Metal/Shaders.metal")]
        ),
        .testTarget(
            name: "SwiftTermTests",
            dependencies: ["SwiftTerm"],
            resources: [
                .copy("Fixtures/xterm-ghostty.infocmp"),
                .copy("Fixtures/swifterm-terminfo.infocmp")
            ]
        )
    ],
    swiftLanguageModes: [.v5]
)
