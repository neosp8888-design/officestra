// 이 파일은 업무 본문과 첨부 파일을 짧은 링크와 이미지 썸네일로 표시한다.

import AppKit
import OfficeCore
import SwiftUI

struct TaskPromptAttachmentList: View {
    let attachments: [TaskPromptAttachment]

    var body: some View {
        VStack(alignment: .leading, spacing: 5) {
            ForEach(attachments, id: \.path) { attachment in
                HStack(spacing: 8) {
                    Button {
                        openTaskAttachmentThumbnail(attachment.path)
                    } label: {
                        TaskAttachmentThumbnail(
                            path: attachment.path,
                            size: 36
                        )
                    }
                    .officeGameTool()
                    .help(taskAttachmentOpenActionTitle(
                        for: attachment.path,
                        isThumbnail: true
                    ))
                    .accessibilityLabel(
                        OfficeLocalization.format(
                            "첨부 썸네일 %@, %@",
                            attachment.name,
                            taskAttachmentOpenActionTitle(
                                for: attachment.path,
                                isThumbnail: true
                            )
                        )
                    )

                    Button {
                        openTaskAttachmentInFinder(attachment.path)
                    } label: {
                        HStack(spacing: 8) {
                        Text(attachment.name)
                            .font(.system(size: 10.5, weight: .semibold))
                            .foregroundStyle(DashboardPalette.accent)
                            .lineLimit(1)

                        Image(systemName: "arrow.up.right.square")
                            .font(.system(size: 9, weight: .semibold))
                            .foregroundStyle(DashboardPalette.accent)

                        Spacer(minLength: 0)
                    }
                    .contentShape(Rectangle())
                    }
                    .buttonStyle(.plain)
                    .help(
                        OfficeLocalization.format(
                            "%@\nFinder에서 보기",
                            attachment.path
                        )
                    )
                    .accessibilityLabel(
                        OfficeLocalization.format(
                            "첨부 파일 %@, Finder에서 보기, 경로 %@",
                            attachment.name,
                            attachment.path
                        )
                    )
                }
                .padding(.horizontal, 8)
                .frame(minHeight: 44)
                .background(
                    Color.primary.opacity(0.045),
                    in: RoundedRectangle(
                        cornerRadius: 8,
                        style: .continuous
                    )
                )
            }
        }
    }
}

struct TaskPromptAttachmentSummary: View {
    let attachments: [TaskPromptAttachment]

    var body: some View {
        if let attachment = attachments.first {
            HStack(spacing: 5) {
                Button {
                    openTaskAttachmentThumbnail(attachment.path)
                } label: {
                    TaskAttachmentThumbnail(
                        path: attachment.path,
                        size: 20
                    )
                }
                .officeGameTool()
                .help(taskAttachmentOpenActionTitle(
                    for: attachment.path,
                    isThumbnail: true
                ))
                .accessibilityLabel(
                    OfficeLocalization.format(
                        "첨부 썸네일 %@, %@",
                        attachment.name,
                        taskAttachmentOpenActionTitle(
                            for: attachment.path,
                            isThumbnail: true
                        )
                    )
                )

                Button {
                    openTaskAttachmentInFinder(attachment.path)
                } label: {
                    HStack(spacing: 5) {
                        Text(attachment.name)
                            .lineLimit(1)

                        if attachments.count > 1 {
                            Text("+\(attachments.count - 1)")
                                .fontWeight(.bold)
                        }
                    }
                }
                .buttonStyle(.plain)
                .help(
                    OfficeLocalization.format(
                        "%@\nFinder에서 보기",
                        attachment.path
                    )
                )
                .accessibilityLabel(
                    OfficeLocalization.format(
                        "첨부 파일 %@, Finder에서 보기, 경로 %@",
                        attachment.name,
                        attachment.path
                    )
                )
            }
            .font(.system(size: 8.5, weight: .medium))
            .foregroundStyle(.secondary)
        }
    }
}

private struct TaskAttachmentThumbnail: View {
    let path: String
    let size: CGFloat
    @State private var image: NSImage?

    var body: some View {
        Group {
            if let image {
                Image(nsImage: image)
                    .resizable()
                    .aspectRatio(contentMode: .fill)
                    .frame(width: size, height: size)
                    .clipped()
            } else {
                Image(systemName: taskAttachmentIconName(for: path))
                    .font(
                        .system(
                            size: max(10, size * 0.42),
                            weight: .semibold
                        )
                    )
                    .foregroundStyle(DashboardPalette.accent)
                    .frame(width: size, height: size)
                    .background(Color.primary.opacity(0.04))
            }
        }
        .clipShape(
            RoundedRectangle(cornerRadius: 6, style: .continuous)
        )
        .overlay {
            RoundedRectangle(cornerRadius: 6, style: .continuous)
                .stroke(Color.primary.opacity(0.10))
        }
        .accessibilityHidden(true)
        .task(id: path) {
            await loadImageIfNeeded()
        }
    }

    @MainActor
    private func loadImageIfNeeded() async {
        guard taskAttachmentIsImage(path) else {
            return
        }
        let url = URL(fileURLWithPath: path)
        if let cached = LocalMarkdownImageCache.shared.image(at: url) {
            image = cached
            return
        }

        let data = await Task.detached(priority: .utility) {
            try? Data(contentsOf: url, options: .mappedIfSafe)
        }.value
        guard
            !Task.isCancelled,
            let data,
            let loadedImage = NSImage(data: data)
        else {
            return
        }
        LocalMarkdownImageCache.shared.store(loadedImage, at: url)
        image = loadedImage
    }
}

private func openTaskAttachmentThumbnail(_ path: String) {
    let url = URL(fileURLWithPath: path)
    guard taskAttachmentIsImage(path) else {
        openTaskAttachmentInFinder(path)
        return
    }

    Task { @MainActor in
        guard let previewURL = NSWorkspace.shared.urlForApplication(
            withBundleIdentifier: "com.apple.Preview"
        ) else {
            openTaskAttachmentInFinder(path)
            return
        }
        do {
            try await NSWorkspace.shared.open(
                [url],
                withApplicationAt: previewURL,
                configuration: .init()
            )
        } catch {
            openTaskAttachmentInFinder(path)
        }
    }
}

private func openTaskAttachmentInFinder(_ path: String) {
    NSWorkspace.shared.activateFileViewerSelecting([URL(fileURLWithPath: path)])
}

func taskAttachmentOpenActionTitle(
    for path: String,
    isThumbnail: Bool
) -> String {
    isThumbnail && taskAttachmentIsImage(path)
        ? OfficeLocalization.string("미리보기에서 열기")
        : OfficeLocalization.string("Finder에서 보기")
}

private func taskAttachmentIsImage(_ path: String) -> Bool {
    switch URL(fileURLWithPath: path).pathExtension.lowercased() {
    case "png", "jpg", "jpeg", "gif", "heic", "webp", "tiff":
        true
    default:
        false
    }
}

func taskAttachmentIconName(for path: String) -> String {
    switch URL(fileURLWithPath: path).pathExtension.lowercased() {
    case "png", "jpg", "jpeg", "gif", "heic", "webp", "tiff":
        "photo.fill"
    case "pdf":
        "doc.richtext.fill"
    case "mov", "mp4", "m4v", "avi", "webm":
        "film.fill"
    case "mp3", "m4a", "wav", "aac", "flac":
        "waveform"
    case "csv", "tsv", "xls", "xlsx", "numbers":
        "tablecells.fill"
    case "zip", "gz", "tar", "7z":
        "archivebox.fill"
    case "swift", "js", "mjs", "ts", "tsx", "py", "go", "rs", "java":
        "chevron.left.forwardslash.chevron.right"
    default:
        "doc.fill"
    }
}
