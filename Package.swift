// swift-tools-version: 5.10

import PackageDescription

let package = Package(
    name: "OfficeLLM",
    defaultLocalization: "en",
    platforms: [
        .macOS(.v14)
    ],
    products: [
        .library(name: "OfficeCore", targets: ["OfficeCore"]),
        .executable(name: "OfficeLLM", targets: ["OfficeGame"])
    ],
    dependencies: [
        .package(
            url: "https://github.com/gonzalezreal/swift-markdown-ui",
            exact: "2.4.1"
        ),
        // Pinned upstream sources with the wide-character overwrite fix.
        .package(path: "Vendor/SwiftTerm")
    ],
    targets: [
        .target(
            name: "OfficeCore",
            exclude: [
                "Resources/character-faces",
                "Resources/office-2d-motion-v1",
                "Resources/office-2d-themes-v1",
                "Resources/office-3d-motion-v1",
                "Resources/office-background-3d-v4.png",
                "Resources/office-theme-modern-day-v4.png",
                "Resources/office-theme-modern-night-v4.png"
            ],
            resources: [
                .copy("Resources/office-retina-v1"),
                .copy("Resources/avatars"),
                .copy("Resources/profiles"),
                .copy("Resources/characters.json")
            ],
            linkerSettings: [
                .linkedFramework("AppKit")
            ]
        ),
        .executableTarget(
            name: "OfficeGame",
            dependencies: [
                "OfficeCore",
                .product(
                    name: "MarkdownUI",
                    package: "swift-markdown-ui"
                ),
                .product(
                    name: "SwiftTerm",
                    package: "SwiftTerm"
                )
            ],
            resources: [
                .process("Resources")
            ],
            linkerSettings: [
                .linkedFramework("AppKit"),
                .linkedFramework("SpriteKit")
            ]
        ),
        .testTarget(
            name: "OfficeCoreTests",
            dependencies: [
                "OfficeCore", "OfficeGame",
                .product(name: "SwiftTerm", package: "SwiftTerm")
            ]
        )
    ]
)
