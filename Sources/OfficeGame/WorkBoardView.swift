// 프로젝트와 티켓의 수동 관리 화면. 배정만 저장하며 직원 업무를 실행하지 않는다.

import Foundation
import SwiftUI

struct WorkBoardAssignee: Identifiable, Codable, Hashable {
    let id: String
    let name: String
}

struct WorkBoardWindowRoute: Codable, Hashable {
    let projectId: String
    let baseURL: String
    let assignees: [WorkBoardAssignee]
}

private struct WorkBoardSnapshot: Decodable {
    let projects: [WorkBoardProject]
    let tickets: [WorkBoardTicket]
    let goals: [WorkBoardGoal]
    let workRecords: [WorkBoardRecord]
}

private struct WorkBoardRecord: Decodable, Identifiable {
    let id: String
    let ticketId: String?
    let title: String
    let body: String?
    let bodyPreview: String?
    let recordType: String
    let characterId: String?
    let sourceTurnId: String?
    let recordedAt: String
}

private struct WorkBoardRecordSearchResponse: Decodable {
    let records: [WorkBoardRecord]
}

private struct WorkBoardRecordDetailResponse: Decodable {
    let record: WorkBoardRecord
}

private struct WorkBoardActivity: Decodable, Identifiable {
    let id: String
    let kind: String
    let summary: String
    let reportedActorId: String?
    let createdAt: String
}

private struct WorkBoardActivityResponse: Decodable {
    let activity: [WorkBoardActivity]
}

private struct WorkBoardActivityInput: Encodable {
    let kind: String
    let body: String
}

private extension WorkBoardRecord {
    var resultPreview: String {
        let source = bodyPreview ?? body ?? ""
        let result = source.components(separatedBy: "결과\n").last ?? source
        return result.trimmingCharacters(in: .whitespacesAndNewlines)
            .components(separatedBy: .newlines)
            .first { !$0.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty }
            .map { String($0.prefix(150)) } ?? "내용 미확인"
    }
}

private struct WorkBoardProject: Decodable, Identifiable {
    let id: String
    let projectKey: String
    let title: String
    let description: String
}

private struct WorkBoardTicket: Decodable, Identifiable {
    let id: String
    let projectId: String
    let title: String
    let description: String
    let assigneeId: String?
    let completedById: String?
    let verifiedById: String?
    let state: String
    let pushedCommitSha: String?
    let dueDate: String?
    let completionCriteria: String
    let decisionPending: Bool
    let parentTicketId: String?
    let predecessorIds: [String]
    let goalIds: [String]
    let workRecordIds: [String]
}

private struct WorkBoardGoal: Decodable, Identifiable {
    let id: String
    let projectId: String
    let horizon: String
    let periodStart: String
    let title: String
    let description: String
    let metricName: String?
    let metricUnit: String?
    let targetValue: String?
    let actualValue: String?
}

struct WorkBoardTicketInput: Encodable {
    let projectId: String?
    let title: String
    let description: String
    let assigneeId: String?
    let completedById: String?
    let verifiedById: String?
    let state: String
    let pushedCommitSha: String?
    let dueDate: String?
    let completionCriteria: String
    let decisionPending: Bool
    let parentTicketId: String?
    let predecessorIds: [String]
    let goalIds: [String]
    let workRecordIds: [String]

    private enum CodingKeys: String, CodingKey {
        case projectId, title, description, assigneeId, completedById, verifiedById, state, pushedCommitSha, dueDate,
            completionCriteria, decisionPending, parentTicketId, predecessorIds, goalIds, workRecordIds
    }

    func encode(to encoder: Encoder) throws {
        var values = encoder.container(keyedBy: CodingKeys.self)
        if let projectId {
            try values.encode(projectId, forKey: .projectId)
        }
        try values.encode(title, forKey: .title)
        try values.encode(description, forKey: .description)
        try values.encode(assigneeId, forKey: .assigneeId)
        try values.encode(completedById, forKey: .completedById)
        try values.encode(verifiedById, forKey: .verifiedById)
        try values.encode(state, forKey: .state)
        try values.encode(pushedCommitSha, forKey: .pushedCommitSha)
        try values.encode(dueDate, forKey: .dueDate)
        try values.encode(completionCriteria, forKey: .completionCriteria)
        try values.encode(decisionPending, forKey: .decisionPending)
        try values.encode(parentTicketId, forKey: .parentTicketId)
        try values.encode(predecessorIds, forKey: .predecessorIds)
        try values.encode(goalIds, forKey: .goalIds)
        try values.encode(workRecordIds, forKey: .workRecordIds)
    }
}

struct WorkBoardGoalInput: Encodable {
    let projectId: String?
    let horizon: String
    let periodStart: String
    let title: String
    let description: String
    let metricName: String?
    let metricUnit: String?
    let targetValue: String?
    let actualValue: String?

    private enum CodingKeys: String, CodingKey {
        case projectId, horizon, periodStart, title, description,
            metricName, metricUnit, targetValue, actualValue
    }

    func encode(to encoder: Encoder) throws {
        var values = encoder.container(keyedBy: CodingKeys.self)
        if let projectId { try values.encode(projectId, forKey: .projectId) }
        try values.encode(horizon, forKey: .horizon)
        try values.encode(periodStart, forKey: .periodStart)
        try values.encode(title, forKey: .title)
        try values.encode(description, forKey: .description)
        try values.encode(metricName, forKey: .metricName)
        try values.encode(metricUnit, forKey: .metricUnit)
        try values.encode(targetValue, forKey: .targetValue)
        try values.encode(actualValue, forKey: .actualValue)
    }
}

private struct WorkBoardProjectInput: Encodable {
    let projectKey: String?
    let title: String
    let description: String
}

private struct WorkBoardErrorResponse: Decodable {
    let error: String
}

private struct WorkBoardClient {
    let baseURL: URL

    func load() async throws -> WorkBoardSnapshot {
        let (data, response) = try await URLSession.shared.data(
            from: baseURL.appending(path: "api/work-board")
        )
        try validate(response, data: data)
        do {
            return try JSONDecoder().decode(WorkBoardSnapshot.self, from: data)
        } catch DecodingError.keyNotFound(let key, _)
            where ["goals", "workRecords", "decisionPending", "completedById", "verifiedById"].contains(key.stringValue) {
            throw WorkBoardClientError.message(
                "새 업무 보드 데이터를 사용하려면 백엔드를 앱 버튼으로 재시작해 주세요."
            )
        }
    }

    func searchRecords(_ query: String) async throws -> [WorkBoardRecord] {
        var components = URLComponents(
            url: baseURL.appending(path: "api/work-records"),
            resolvingAgainstBaseURL: false
        )!
        components.queryItems = [
            URLQueryItem(name: "q", value: query),
            URLQueryItem(name: "limit", value: "20")
        ]
        let (data, response) = try await URLSession.shared.data(from: components.url!)
        try validate(response, data: data)
        return try JSONDecoder().decode(WorkBoardRecordSearchResponse.self, from: data).records
    }

    func loadRecord(_ id: String) async throws -> WorkBoardRecord {
        let (data, response) = try await URLSession.shared.data(
            from: baseURL.appending(path: "api/work-board/records/\(id)")
        )
        try validate(response, data: data)
        return try JSONDecoder().decode(WorkBoardRecordDetailResponse.self, from: data).record
    }

    func loadActivity(_ ticketId: String) async throws -> [WorkBoardActivity] {
        let (data, response) = try await URLSession.shared.data(
            from: baseURL.appending(path: "api/work-board/tickets/\(ticketId)/activity")
        )
        try validate(response, data: data)
        return try JSONDecoder().decode(WorkBoardActivityResponse.self, from: data).activity
    }

    func addActivity(_ input: WorkBoardActivityInput, ticketId: String) async throws {
        var request = URLRequest(
            url: baseURL.appending(path: "api/work-board/tickets/\(ticketId)/activity")
        )
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "content-type")
        request.httpBody = try JSONEncoder().encode(input)
        let (data, response) = try await URLSession.shared.data(for: request)
        try validate(response, data: data)
    }

    func save(_ input: WorkBoardTicketInput, ticketId: String?) async throws {
        let path = ticketId.map { "api/work-board/tickets/\($0)" }
            ?? "api/work-board/tickets"
        var request = URLRequest(url: baseURL.appending(path: path))
        request.httpMethod = ticketId == nil ? "POST" : "PATCH"
        request.setValue("application/json", forHTTPHeaderField: "content-type")
        request.httpBody = try JSONEncoder().encode(input)
        let (data, response) = try await URLSession.shared.data(for: request)
        try validate(response, data: data)
    }

    func saveProject(_ input: WorkBoardProjectInput, projectId: String?) async throws {
        let path = projectId.map { "api/work-board/projects/\($0)" }
            ?? "api/work-board/projects"
        var request = URLRequest(url: baseURL.appending(path: path))
        request.httpMethod = projectId == nil ? "POST" : "PATCH"
        request.setValue("application/json", forHTTPHeaderField: "content-type")
        request.httpBody = try JSONEncoder().encode(input)
        let (data, response) = try await URLSession.shared.data(for: request)
        try validate(response, data: data)
    }

    func saveGoal(_ input: WorkBoardGoalInput, goalId: String?) async throws {
        let path = goalId.map { "api/work-board/goals/\($0)" }
            ?? "api/work-board/goals"
        var request = URLRequest(url: baseURL.appending(path: path))
        request.httpMethod = goalId == nil ? "POST" : "PATCH"
        request.setValue("application/json", forHTTPHeaderField: "content-type")
        request.httpBody = try JSONEncoder().encode(input)
        let (data, response) = try await URLSession.shared.data(for: request)
        try validate(response, data: data)
    }

    private func validate(_ response: URLResponse, data: Data) throws {
        guard let response = response as? HTTPURLResponse else {
            throw WorkBoardClientError.message("백엔드 응답을 확인할 수 없습니다.")
        }
        guard (200..<300).contains(response.statusCode) else {
            let message = try? JSONDecoder()
                .decode(WorkBoardErrorResponse.self, from: data).error
            throw WorkBoardClientError.message(
                message ?? (response.statusCode == 404
                    ? "업무 보드 API를 사용할 수 없습니다. 백엔드를 앱 버튼으로 재시작해 주세요."
                    : "업무 보드 요청 실패 (HTTP \(response.statusCode))")
            )
        }
    }
}

private enum WorkBoardClientError: LocalizedError {
    case message(String)

    var errorDescription: String? {
        if case .message(let message) = self { return message }
        return nil
    }
}

private enum WorkBoardState: String, CaseIterable, Hashable, Identifiable {
    case open
    case inProgress = "in_progress"
    case review
    case done
    case canceled
    case deferred

    var id: String { rawValue }

    var label: String {
        switch self {
        case .open: "오픈"
        case .inProgress: "진행"
        case .review: "검토"
        case .done: "완료 · 반영"
        case .canceled: "취소"
        case .deferred: "대기"
        }
    }

    var tint: Color {
        switch self {
        case .open: .secondary
        case .inProgress: .blue
        case .review: .orange
        case .done: .green
        case .canceled: .red
        case .deferred: .gray
        }
    }
}

private struct WorkBoardDraft: Identifiable {
    let id: String
    let projectId: String
    let ticket: WorkBoardTicket?

    init(projectId: String, ticket: WorkBoardTicket? = nil) {
        id = ticket?.id ?? UUID().uuidString
        self.projectId = projectId
        self.ticket = ticket
    }
}

private enum WorkBoardDisplayMode: String, CaseIterable, Identifiable {
    case dashboard
    case wbs
    case kanban

    var id: String { rawValue }
    var title: String {
        switch self {
        case .dashboard: "대시보드"
        case .wbs: "WBS"
        case .kanban: "칸반"
        }
    }
}

private struct WorkBoardWBSRow: Identifiable {
    let ticket: WorkBoardTicket
    let depth: Int
    var id: String { ticket.id }
}

private struct WorkBoardSurface: ViewModifier {
    func body(content: Content) -> some View {
        content
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(14)
            .background(
                Color(nsColor: .textBackgroundColor).opacity(0.7),
                in: RoundedRectangle(cornerRadius: 10, style: .continuous)
            )
            .overlay {
                RoundedRectangle(cornerRadius: 10, style: .continuous)
                    .stroke(Color.primary.opacity(0.07))
            }
    }
}

private extension View {
    func workBoardSurface() -> some View { modifier(WorkBoardSurface()) }
}

private struct WorkBoardActionStyle: ButtonStyle {
    var prominent = false
    @Environment(\.isEnabled) private var isEnabled

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .foregroundStyle(prominent ? Color.white : DashboardPalette.accent)
            .padding(.horizontal, 12)
            .background(
                prominent
                    ? DashboardPalette.accent.opacity(configuration.isPressed ? 0.82 : 1)
                    : Color(nsColor: .controlBackgroundColor).opacity(configuration.isPressed ? 0.65 : 0.9),
                in: RoundedRectangle(cornerRadius: 11, style: .continuous)
            )
            .overlay {
                RoundedRectangle(cornerRadius: 11, style: .continuous)
                    .strokeBorder(prominent ? Color.clear : DashboardPalette.accent.opacity(0.16))
            }
            .opacity(isEnabled ? 1 : 0.45)
    }
}

// 좁은 정보 패널에는 프로젝트 요약만 놓고, 상세는 별도 큰 창에서 연다.
struct WorkBoardProjectLauncher: View {
    let databaseBaseURL: URL
    let assignees: [WorkBoardAssignee]

    @Environment(\.openWindow) private var openWindow
    @State private var snapshot: WorkBoardSnapshot?
    @State private var projectDraft: WorkBoardProjectDraft?
    @State private var errorMessage: String?
    @State private var isLoading = false

    var body: some View {
        VStack(alignment: .leading, spacing: 14) {
            HStack {
                Label("프로젝트 현황", systemImage: "square.grid.2x2.fill")
                    .font(.system(size: 13, weight: .bold))
                    .foregroundStyle(DashboardPalette.accent)
                Spacer()
                Button {
                    projectDraft = WorkBoardProjectDraft(project: nil)
                } label: {
                    Image(systemName: "plus.square")
                }
                .buttonStyle(.bordered)
                .help("프로젝트 추가")
                .accessibilityIdentifier("workBoardAddProject")
                Button {
                    Task { await reload() }
                } label: {
                    Image(systemName: "arrow.clockwise")
                }
                .buttonStyle(.bordered)
                .disabled(isLoading)
                .help("프로젝트 새로고침")
                .accessibilityIdentifier("workBoardRefresh")
            }
            Text("프로젝트를 누르면 넓은 업무 창에서 대시보드·WBS·칸반을 볼 수 있습니다.")
                .font(.caption)
                .foregroundStyle(.secondary)
            if let errorMessage {
                Text(errorMessage).font(.caption).foregroundStyle(.red)
            }
            if isLoading && snapshot == nil {
                ProgressView().frame(maxWidth: .infinity, maxHeight: .infinity)
            } else if let snapshot {
                ScrollView {
                    LazyVStack(spacing: 9) {
                        ForEach(snapshot.projects) { project in
                            let tickets = snapshot.tickets.filter { $0.projectId == project.id }
                            Button {
                                openWindow(id: "work-board-project", value: WorkBoardWindowRoute(
                                    projectId: project.id,
                                    baseURL: databaseBaseURL.absoluteString,
                                    assignees: assignees
                                ))
                            } label: {
                                VStack(alignment: .leading, spacing: 8) {
                                    HStack {
                                        Text(project.title)
                                            .font(.system(size: 13, weight: .bold))
                                            .foregroundStyle(DashboardPalette.accent)
                                        Spacer()
                                        Image(systemName: "arrow.up.forward.square")
                                            .foregroundStyle(DashboardPalette.accent)
                                    }
                                    Text(tickets.isEmpty
                                         ? "현황 미확인"
                                         : "오픈 \(tickets.filter { $0.state == "open" }.count) · 진행 \(tickets.filter { $0.state == "in_progress" }.count) · 검토 \(tickets.filter { $0.state == "review" }.count) · 반영 완료 \(tickets.filter { $0.state == "done" }.count) · 대기/취소 \(tickets.filter { ["deferred", "canceled"].contains($0.state) }.count)")
                                        .font(.caption)
                                        .foregroundStyle(.secondary)
                                    if !project.description.isEmpty {
                                        Text(project.description)
                                            .font(.caption2)
                                            .foregroundStyle(.secondary)
                                            .lineLimit(2)
                                    }
                                }
                                .frame(maxWidth: .infinity, alignment: .leading)
                            .padding(14)
                            .background(
                                Color(nsColor: .textBackgroundColor).opacity(0.7),
                                in: RoundedRectangle(cornerRadius: 14, style: .continuous)
                            )
                            .overlay {
                                RoundedRectangle(cornerRadius: 14, style: .continuous)
                                    .stroke(DashboardPalette.accent.opacity(0.13))
                            }
                            }
                            .buttonStyle(.plain)
                            .accessibilityIdentifier("workBoardProject-\(project.projectKey)")
                        }
                    }
                }
            } else {
                Text("프로젝트를 불러오지 못했습니다.")
                    .foregroundStyle(.secondary)
            }
        }
        .padding(14)
        .background(
            LinearGradient(
                colors: [DashboardPalette.accent.opacity(0.055), Color.primary.opacity(0.012)],
                startPoint: .topLeading,
                endPoint: .bottomTrailing
            )
        )
        .task { await reload() }
        .sheet(item: $projectDraft) { draft in
            WorkBoardProjectEditor(
                client: WorkBoardClient(baseURL: databaseBaseURL),
                draft: draft,
                onSaved: { Task { await reload() } }
            )
        }
    }

    @MainActor
    private func reload() async {
        guard !isLoading else { return }
        isLoading = true
        defer { isLoading = false }
        do {
            snapshot = try await WorkBoardClient(baseURL: databaseBaseURL).load()
            errorMessage = nil
        } catch {
            errorMessage = error.localizedDescription
        }
    }
}

struct WorkBoardView: View {
    let databaseBaseURL: URL
    let assignees: [WorkBoardAssignee]
    let initialProjectId: String

    @State private var snapshot: WorkBoardSnapshot?
    @State private var selectedProjectId: String?
    @State private var draft: WorkBoardDraft?
    @State private var projectDraft: WorkBoardProjectDraft?
    @State private var goalDraft: WorkBoardGoalDraft?
    @State private var displayMode: WorkBoardDisplayMode = .dashboard
    @State private var isLoading = false
    @State private var errorMessage: String?

    init(databaseBaseURL: URL, assignees: [WorkBoardAssignee], initialProjectId: String) {
        self.databaseBaseURL = databaseBaseURL
        self.assignees = assignees
        self.initialProjectId = initialProjectId
        _selectedProjectId = State(initialValue: initialProjectId)
    }

    private var client: WorkBoardClient {
        WorkBoardClient(baseURL: databaseBaseURL)
    }

    private var selectedProject: WorkBoardProject? {
        snapshot?.projects.first { $0.id == selectedProjectId }
    }

    private var selectedTickets: [WorkBoardTicket] {
        snapshot?.tickets.filter { $0.projectId == selectedProjectId } ?? []
    }

    private var selectedGoals: [WorkBoardGoal] {
        snapshot?.goals.filter { $0.projectId == selectedProjectId } ?? []
    }

    private var wbsRows: [WorkBoardWBSRow] {
        let tickets = selectedTickets
        let ids = Set(tickets.map(\.id))
        var rows: [WorkBoardWBSRow] = []
        var visited = Set<String>()
        func append(_ ticket: WorkBoardTicket, depth: Int) {
            guard visited.insert(ticket.id).inserted else { return }
            rows.append(WorkBoardWBSRow(ticket: ticket, depth: depth))
            for child in tickets where child.parentTicketId == ticket.id {
                append(child, depth: depth + 1)
            }
        }
        for ticket in tickets where ticket.parentTicketId.map({ !ids.contains($0) }) ?? true {
            append(ticket, depth: 0)
        }
        for ticket in tickets where !visited.contains(ticket.id) {
            append(ticket, depth: 0)
        }
        return rows
    }

    var body: some View {
        VStack(spacing: 0) {
            boardToolbar
            Divider().opacity(0.5)
            if let errorMessage {
                Text(errorMessage)
                    .font(.caption)
                    .foregroundStyle(.red)
                    .padding(.horizontal, 20)
            }
            if isLoading && snapshot == nil {
                ProgressView("프로젝트를 불러오는 중")
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            } else if let snapshot {
                ScrollView {
                    VStack(alignment: .leading, spacing: 18) {
                        boardActions
                        HStack(alignment: .top, spacing: 12) {
                            ForEach(snapshot.projects) { project in
                                projectButton(project, tickets: snapshot.tickets)
                            }
                        }
                        projectHeading
                        HStack {
                            Picker("보기", selection: $displayMode) {
                                ForEach(WorkBoardDisplayMode.allCases) { mode in
                                    Text(mode.title).tag(mode)
                                }
                            }
                            .pickerStyle(.segmented)
                            .labelsHidden()
                            .frame(width: 330)
                            .accessibilityIdentifier("workBoardDisplayMode")
                            Spacer()
                            Text("티켓 \(selectedTickets.count)건 · 완료 개수와 KPI는 별개")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                        }
                        boardModeContent
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(20)
                }
            } else {
                ContentUnavailableView("프로젝트를 불러오지 못했습니다", systemImage: "square.grid.2x2")
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            }
        }
        .background(
            LinearGradient(
                colors: [DashboardPalette.accent.opacity(0.055), Color.primary.opacity(0.012)],
                startPoint: .topLeading,
                endPoint: .bottomTrailing
            )
        )
        .frame(minWidth: 1050, minHeight: 680)
        .task { await reload() }
        .sheet(item: $draft) { draft in
            WorkBoardTicketEditor(
                client: client,
                draft: draft,
                projectKey: selectedProject?.projectKey ?? "",
                assignees: assignees,
                tickets: selectedTickets,
                goals: selectedGoals,
                workRecords: snapshot?.workRecords ?? [],
                onSaved: { Task { await reload() } }
            )
        }
        .sheet(item: $projectDraft) { draft in
            WorkBoardProjectEditor(
                client: client,
                draft: draft,
                onSaved: { Task { await reload() } }
            )
        }
        .sheet(item: $goalDraft) { draft in
            WorkBoardGoalEditor(
                client: client,
                draft: draft,
                onSaved: { Task { await reload() } }
            )
        }
    }

    private var boardToolbar: some View {
        HStack(spacing: 12) {
            Label("프로젝트 현황", systemImage: "square.grid.2x2.fill")
                .font(.system(size: 13, weight: .bold))
                .foregroundStyle(DashboardPalette.accent)
            Spacer()
            Text("프로젝트와 진행 작업을 한곳에서 관리합니다")
                .font(.caption)
                .foregroundStyle(.secondary)
        }
        .padding(.horizontal, 20)
        .frame(height: 54)
    }

    private var boardActions: some View {
        HStack(spacing: 10) {
            Button {
                projectDraft = WorkBoardProjectDraft(project: nil)
            } label: {
                boardActionLabel("프로젝트 추가", systemImage: "folder.badge.plus")
            }
            .buttonStyle(WorkBoardActionStyle())
            .accessibilityIdentifier("workBoardAddProject")
            Button {
                Task { await reload() }
            } label: {
                boardActionLabel("새로고침", systemImage: "arrow.clockwise")
            }
            .buttonStyle(WorkBoardActionStyle())
            .disabled(isLoading)
            .accessibilityIdentifier("workBoardRefresh")
            Button {
                if let selectedProject {
                    projectDraft = WorkBoardProjectDraft(project: selectedProject)
                }
            } label: {
                boardActionLabel("프로젝트 수정", systemImage: "square.and.pencil")
            }
            .buttonStyle(WorkBoardActionStyle())
            .disabled(selectedProject == nil)
            .accessibilityIdentifier("workBoardEditProject")
            Button {
                if let selectedProjectId {
                    draft = WorkBoardDraft(projectId: selectedProjectId)
                }
            } label: {
                boardActionLabel("티켓 추가", systemImage: "plus.circle.fill")
            }
            .buttonStyle(WorkBoardActionStyle(prominent: true))
            .disabled(selectedProjectId == nil)
            .accessibilityIdentifier("workBoardAddTicket")
        }
        .accessibilityIdentifier("workBoardActions")
    }

    private func boardActionLabel(_ title: String, systemImage: String) -> some View {
        Label(title, systemImage: systemImage)
            .font(.system(size: 13, weight: .semibold))
            .frame(maxWidth: .infinity, minHeight: 40)
            .contentShape(Rectangle())
    }

    private var projectHeading: some View {
        HStack(alignment: .center) {
            VStack(alignment: .leading, spacing: 3) {
                Text(selectedProject?.title ?? "프로젝트 선택")
                    .font(.system(size: 21, weight: .bold, design: .rounded))
                Text("작업 상태와 담당, 일정, 완료 근거를 한곳에서 봅니다.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Spacer()
        }
    }

    @ViewBuilder
    private var boardModeContent: some View {
        switch displayMode {
        case .dashboard:
            dashboardContent
        case .wbs:
            LazyVStack(alignment: .leading, spacing: 8) {
                Label("작업 구조", systemImage: "list.bullet.indent")
                    .font(.system(size: 13, weight: .bold))
                    .foregroundStyle(DashboardPalette.accent)
                HStack(spacing: 8) {
                    Text("작업 계층").frame(maxWidth: .infinity, alignment: .leading)
                    Text("상태").frame(width: 70)
                    Text("담당").frame(width: 92)
                    Text("완료자").frame(width: 92)
                    Text("검증자").frame(width: 92)
                    Text("기한").frame(width: 100)
                }
                .font(.caption.bold())
                .foregroundStyle(.secondary)
                .padding(.horizontal, 12)
                ForEach(wbsRows) { row in wbsRow(row) }
            }
            .workBoardSurface()
            .accessibilityIdentifier("workBoardWBS")
        case .kanban:
            ScrollView(.horizontal) {
                HStack(alignment: .top, spacing: 12) {
                    ForEach(WorkBoardState.allCases) { state in
                        stateSection(state)
                            .frame(width: 265, alignment: .topLeading)
                    }
                }
            }
            .accessibilityIdentifier("workBoardKanban")
        }
    }

    private var dashboardContent: some View {
        VStack(alignment: .leading, spacing: 16) {
            if let selectedProject, !selectedProject.description.isEmpty {
                Text(selectedProject.description)
                    .font(.system(size: 12))
                    .foregroundStyle(.secondary)
                    .textSelection(.enabled)
                    .workBoardSurface()
            }
            HStack(spacing: 10) {
                ForEach(WorkBoardState.allCases) { state in
                    let count = selectedTickets.filter { $0.state == state.rawValue }.count
                    VStack(alignment: .leading, spacing: 6) {
                        HStack(spacing: 6) {
                            Circle().fill(state.tint).frame(width: 7, height: 7)
                            Text(state.label)
                                .font(.system(size: 11, weight: .bold))
                                .foregroundStyle(.secondary)
                        }
                        Text("\(count)건")
                            .font(.system(size: 22, weight: .bold, design: .rounded))
                            .monospacedDigit()
                    }
                    .workBoardSurface()
                }
            }
            HStack(alignment: .top, spacing: 16) {
                VStack(alignment: .leading, spacing: 8) {
                    Label("진행할 일", systemImage: "bolt.fill")
                        .font(.system(size: 13, weight: .bold))
                        .foregroundStyle(DashboardPalette.accent)
                    let openTickets = selectedTickets.filter { ["open", "in_progress", "review"].contains($0.state) }
                    if openTickets.isEmpty {
                        Text("등록된 진행 작업 없음").font(.caption).foregroundStyle(.secondary)
                    }
                    ForEach(openTickets) { ticket in ticketButton(ticket) }
                }
                .workBoardSurface()
                VStack(alignment: .leading, spacing: 8) {
                    Label("최근 반영 완료", systemImage: "checkmark.circle.fill")
                        .font(.system(size: 13, weight: .bold))
                        .foregroundStyle(DashboardPalette.accent)
                    ForEach(Array(selectedTickets.filter { $0.state == WorkBoardState.done.rawValue }.reversed().prefix(8))) { ticket in
                        ticketButton(ticket)
                    }
                    if selectedTickets.contains(where: { $0.state == WorkBoardState.done.rawValue }) {
                        Text("나머지 완료 티켓은 WBS·칸반에서 확인")
                            .font(.caption).foregroundStyle(.secondary)
                    }
                }
                .workBoardSurface()
            }
            if selectedTickets.contains(where: { ["deferred", "canceled"].contains($0.state) }) {
                VStack(alignment: .leading, spacing: 8) {
                    Label("대기 · 취소", systemImage: "pause.circle")
                        .font(.system(size: 13, weight: .bold))
                        .foregroundStyle(DashboardPalette.accent)
                    ForEach(selectedTickets.filter { ["deferred", "canceled"].contains($0.state) }) { ticket in
                        ticketButton(ticket)
                    }
                }
                .workBoardSurface()
            }
            VStack(alignment: .leading, spacing: 8) {
                HStack {
                    Label("주간·월간 목표", systemImage: "target")
                        .font(.system(size: 13, weight: .bold))
                        .foregroundStyle(DashboardPalette.accent)
                    Spacer()
                    Button {
                        if let selectedProjectId {
                            goalDraft = WorkBoardGoalDraft(projectId: selectedProjectId)
                        }
                    } label: {
                        Label("목표 추가", systemImage: "plus")
                    }
                    .buttonStyle(.bordered)
                    .disabled(selectedProjectId == nil)
                    .accessibilityIdentifier("workBoardAddGoal")
                }
                if selectedGoals.isEmpty {
                    Text("목표 미정").font(.caption).foregroundStyle(.secondary)
                }
                ForEach(selectedGoals) { goal in goalButton(goal) }
            }
            .workBoardSurface()
            Text("티켓 완료 개수는 KPI 달성률이 아닙니다.")
                .font(.caption).foregroundStyle(.secondary)
        }
        .accessibilityIdentifier("workBoardDashboard")
    }

    private func projectButton(
        _ project: WorkBoardProject,
        tickets: [WorkBoardTicket]
    ) -> some View {
        let projectTickets = tickets.filter { $0.projectId == project.id }
        let selected = selectedProjectId == project.id
        let doneCount = projectTickets.filter { $0.state == WorkBoardState.done.rawValue }.count
        return Button {
            selectedProjectId = project.id
        } label: {
            VStack(alignment: .leading, spacing: 9) {
                HStack {
                    Text(project.title)
                        .font(.system(size: 13, weight: .bold))
                        .foregroundStyle(selected ? DashboardPalette.accent : .primary)
                        .lineLimit(1)
                    Spacer()
                    if selected {
                        Image(systemName: "checkmark.circle.fill")
                            .foregroundStyle(DashboardPalette.accent)
                    }
                }
                Text(projectTickets.isEmpty ? "현황 미확인" : "전체 \(projectTickets.count) · 완료 \(doneCount)")
                    .font(.system(size: 11))
                    .foregroundStyle(.secondary)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(14)
            .background(
                selected ? DashboardPalette.accent.opacity(0.09)
                    : Color(nsColor: .textBackgroundColor).opacity(0.7),
                in: RoundedRectangle(cornerRadius: 10, style: .continuous)
            )
            .overlay {
                RoundedRectangle(cornerRadius: 10, style: .continuous)
                    .stroke(selected ? DashboardPalette.accent.opacity(0.35) : Color.primary.opacity(0.07))
            }
        }
        .buttonStyle(.plain)
        .accessibilityIdentifier("workBoardProject-\(project.projectKey)")
    }

    private func stateSection(_ state: WorkBoardState) -> some View {
        let tickets = selectedTickets.filter { $0.state == state.rawValue }
        return VStack(alignment: .leading, spacing: 9) {
            HStack(spacing: 7) {
                Circle().fill(state.tint).frame(width: 8, height: 8)
                Text(state.label).font(.system(size: 13, weight: .bold))
                Spacer()
                Text("\(tickets.count)")
                    .font(.system(size: 11, weight: .bold, design: .rounded))
                    .monospacedDigit()
                    .foregroundStyle(.secondary)
            }
            ForEach(tickets) { ticket in
                ticketButton(ticket)
            }
        }
        .frame(maxWidth: .infinity, alignment: .topLeading)
        .workBoardSurface()
    }

    private func wbsRow(_ row: WorkBoardWBSRow) -> some View {
        let ticket = row.ticket
        let name = { (id: String?) in assignees.first { $0.id == id }?.name ?? "미확인" }
        return Button {
            draft = WorkBoardDraft(projectId: ticket.projectId, ticket: ticket)
        } label: {
            HStack(spacing: 8) {
                HStack(spacing: 6) {
                    if row.depth > 0 {
                        Image(systemName: "arrow.turn.down.right")
                            .foregroundStyle(.tertiary)
                    }
                    VStack(alignment: .leading, spacing: 2) {
                        Text(ticket.title).font(.system(size: 12, weight: row.depth == 0 ? .semibold : .regular))
                        if !ticket.predecessorIds.isEmpty || !ticket.goalIds.isEmpty {
                            Text("선행 \(ticket.predecessorIds.count) · 목표 \(ticket.goalIds.count)")
                                .font(.caption2).foregroundStyle(.secondary)
                        }
                    }
                    Spacer(minLength: 0)
                }
                .padding(.leading, CGFloat(min(row.depth, 8)) * 16)
                .frame(maxWidth: .infinity, alignment: .leading)
                Text(WorkBoardState(rawValue: ticket.state)?.label ?? ticket.state)
                    .foregroundStyle(WorkBoardState(rawValue: ticket.state)?.tint ?? .secondary)
                    .frame(width: 70)
                Text(assignees.first { $0.id == ticket.assigneeId }?.name ?? "미정")
                    .frame(width: 92)
                Text(["review", "done"].contains(ticket.state) ? name(ticket.completedById) : "—")
                    .frame(width: 92)
                Text(["review", "done"].contains(ticket.state) ? name(ticket.verifiedById) : "—")
                    .frame(width: 92)
                Text(ticket.dueDate ?? "미정").frame(width: 100)
            }
            .font(.caption)
            .padding(.horizontal, 12)
            .padding(.vertical, 9)
            .background(Color.primary.opacity(0.035), in: RoundedRectangle(cornerRadius: 7))
            .contentShape(Rectangle())
        }
        .buttonStyle(.plain)
        .accessibilityIdentifier("workBoardTicket-\(ticket.id)")
    }

    private func ticketButton(_ ticket: WorkBoardTicket, depth: Int = 0) -> some View {
        let predecessorNames = selectedTickets
            .filter { ticket.predecessorIds.contains($0.id) }
            .map(\.title)
            .joined(separator: ", ")
        let goalNames = selectedGoals
            .filter { ticket.goalIds.contains($0.id) }
            .map(\.title)
            .joined(separator: ", ")
        let linkedRecords = snapshot?.workRecords.filter { $0.ticketId == ticket.id } ?? []
        return Button {
            draft = WorkBoardDraft(projectId: ticket.projectId, ticket: ticket)
        } label: {
            VStack(alignment: .leading, spacing: 7) {
                HStack(spacing: 5) {
                    if depth > 0 {
                        Image(systemName: "arrow.turn.down.right")
                            .foregroundStyle(.secondary)
                    }
                    Text(ticket.title)
                        .font(.system(size: 12, weight: .semibold))
                        .lineLimit(2)
                }
                HStack(spacing: 8) {
                    Text(WorkBoardState(rawValue: ticket.state)?.label ?? ticket.state)
                        .foregroundStyle(WorkBoardState(rawValue: ticket.state)?.tint ?? .secondary)
                    Text(assignees.first { $0.id == ticket.assigneeId }?.name ?? "담당 미정")
                    Text(ticket.dueDate ?? "기한 미정")
                }
                .font(.system(size: 11))
                if [WorkBoardState.review.rawValue, WorkBoardState.done.rawValue].contains(ticket.state) {
                    Text("완료: \(assignees.first { $0.id == ticket.completedById }?.name ?? "미확인") · 검증: \(assignees.first { $0.id == ticket.verifiedById }?.name ?? "미확인")")
                        .font(.system(size: 11))
                        .foregroundStyle(.secondary)
                }
                if let sha = ticket.pushedCommitSha {
                    Text("푸시 커밋: \(sha.prefix(12))")
                        .font(.system(size: 10, design: .monospaced))
                        .foregroundStyle(.secondary)
                }
                if ticket.decisionPending {
                    Label("사용자 결정 대기", systemImage: "questionmark.circle")
                        .font(.system(size: 11, weight: .medium))
                        .foregroundStyle(.orange)
                }
                if !predecessorNames.isEmpty {
                    Text("선행: \(predecessorNames)")
                        .font(.system(size: 11))
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
                if !goalNames.isEmpty {
                    Text("목표: \(goalNames)")
                        .font(.system(size: 11))
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                }
                if !linkedRecords.isEmpty {
                    Text("기록 당시 결과: \(linkedRecords[0].resultPreview)")
                        .font(.system(size: 11))
                        .foregroundStyle(.secondary)
                        .lineLimit(2)
                    Text("연결 기록 \(linkedRecords.count)건 · 현재 실행 상태와 별개")
                        .font(.system(size: 10))
                        .foregroundStyle(.tertiary)
                }
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(11)
            .background(Color.primary.opacity(0.035), in: RoundedRectangle(cornerRadius: 8))
            .overlay {
                RoundedRectangle(cornerRadius: 8)
                    .stroke(Color.primary.opacity(0.06))
            }
            .padding(.leading, CGFloat(min(depth, 8)) * 12)
        }
        .buttonStyle(.plain)
        .accessibilityIdentifier("workBoardTicket-\(ticket.id)")
    }

    private func goalButton(_ goal: WorkBoardGoal) -> some View {
        Button {
            goalDraft = WorkBoardGoalDraft(projectId: goal.projectId, goal: goal)
        } label: {
            VStack(alignment: .leading, spacing: 2) {
                Text(goal.title).font(.system(size: 10, weight: .semibold)).lineLimit(1)
                Text("\(goal.horizon == "weekly" ? "주간" : "월간") · \(goal.periodStart)")
                    .font(.system(size: 9))
                    .foregroundStyle(.secondary)
                Text(goal.targetValue.map {
                    "KPI \(goal.actualValue ?? "미입력") / \($0) \(goal.metricUnit ?? "")"
                } ?? "KPI 수치 미입력")
                .font(.system(size: 9))
                .foregroundStyle(.secondary)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding(7)
            .background(Color.primary.opacity(0.045), in: RoundedRectangle(cornerRadius: 7))
        }
        .buttonStyle(.plain)
        .accessibilityIdentifier("workBoardGoal-\(goal.id)")
    }

    @MainActor
    private func reload() async {
        guard !isLoading else { return }
        isLoading = true
        defer { isLoading = false }
        do {
            let next = try await client.load()
            snapshot = next
            if !next.projects.contains(where: { $0.id == selectedProjectId }) {
                selectedProjectId = next.projects.first?.id
            }
            errorMessage = nil
        } catch {
            errorMessage = error.localizedDescription
        }
    }
}

private struct WorkBoardProjectDraft: Identifiable {
    let id: String
    let project: WorkBoardProject?

    init(project: WorkBoardProject?) {
        id = project?.id ?? UUID().uuidString
        self.project = project
    }
}

private struct WorkBoardProjectEditor: View {
    let client: WorkBoardClient
    let draft: WorkBoardProjectDraft
    let onSaved: () -> Void

    @Environment(\.dismiss) private var dismiss
    @State private var projectKey: String
    @State private var title: String
    @State private var description: String
    @State private var isSaving = false
    @State private var errorMessage: String?

    init(client: WorkBoardClient, draft: WorkBoardProjectDraft, onSaved: @escaping () -> Void) {
        self.client = client
        self.draft = draft
        self.onSaved = onSaved
        _projectKey = State(initialValue: draft.project?.projectKey ?? "")
        _title = State(initialValue: draft.project?.title ?? "")
        _description = State(initialValue: draft.project?.description ?? "")
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 13) {
            Text(draft.project == nil ? "프로젝트 만들기" : "프로젝트 수정")
                .font(.title3.bold())
            if draft.project == nil {
                TextField("고유 키 (소문자·숫자·하이픈)", text: $projectKey)
            } else {
                Text("키: \(projectKey)")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            TextField("이름", text: $title)
            TextField("설명", text: $description, axis: .vertical)
                .lineLimit(2...4)
            if let errorMessage {
                Text(errorMessage).foregroundStyle(.red).font(.caption)
            }
            HStack {
                Spacer()
                Button("취소") { dismiss() }
                Button("저장") { Task { await save() } }
                    .buttonStyle(.borderedProminent)
                    .disabled(isSaving || title.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                    .accessibilityIdentifier("workBoardSaveProject")
            }
        }
        .padding(22)
        .frame(width: 440)
    }

    @MainActor
    private func save() async {
        guard !isSaving else { return }
        isSaving = true
        defer { isSaving = false }
        do {
            try await client.saveProject(
                WorkBoardProjectInput(
                    projectKey: draft.project == nil ? projectKey : nil,
                    title: title,
                    description: description
                ),
                projectId: draft.project?.id
            )
            dismiss()
            onSaved()
        } catch {
            errorMessage = error.localizedDescription
        }
    }
}

private struct WorkBoardGoalDraft: Identifiable {
    let id: String
    let projectId: String
    let goal: WorkBoardGoal?

    init(projectId: String, goal: WorkBoardGoal? = nil) {
        id = goal?.id ?? UUID().uuidString
        self.projectId = projectId
        self.goal = goal
    }
}

private struct WorkBoardGoalEditor: View {
    let client: WorkBoardClient
    let draft: WorkBoardGoalDraft
    let onSaved: () -> Void

    @Environment(\.dismiss) private var dismiss
    @State private var horizon: String
    @State private var periodStart: String
    @State private var title: String
    @State private var description: String
    @State private var metricName: String
    @State private var metricUnit: String
    @State private var targetValue: String
    @State private var actualValue: String
    @State private var isSaving = false
    @State private var errorMessage: String?

    init(client: WorkBoardClient, draft: WorkBoardGoalDraft, onSaved: @escaping () -> Void) {
        self.client = client
        self.draft = draft
        self.onSaved = onSaved
        _horizon = State(initialValue: draft.goal?.horizon ?? "weekly")
        _periodStart = State(initialValue: draft.goal?.periodStart ?? "")
        _title = State(initialValue: draft.goal?.title ?? "")
        _description = State(initialValue: draft.goal?.description ?? "")
        _metricName = State(initialValue: draft.goal?.metricName ?? "")
        _metricUnit = State(initialValue: draft.goal?.metricUnit ?? "")
        _targetValue = State(initialValue: draft.goal?.targetValue ?? "")
        _actualValue = State(initialValue: draft.goal?.actualValue ?? "")
    }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(draft.goal == nil ? "목표 만들기" : "목표 수정")
                .font(.title3.bold())
            Picker("기간", selection: $horizon) {
                Text("주간").tag("weekly")
                Text("월간").tag("monthly")
            }
            TextField("시작일 YYYY-MM-DD · 주간은 월요일, 월간은 1일", text: $periodStart)
            TextField("목표 제목", text: $title)
            TextField("설명", text: $description, axis: .vertical)
                .lineLimit(2...3)
            Divider()
            Text("KPI 수치 · 선택 사항")
                .font(.caption.bold())
            TextField("지표 이름", text: $metricName)
            HStack {
                TextField("목표값", text: $targetValue)
                TextField("실제값", text: $actualValue)
                TextField("단위", text: $metricUnit)
            }
            Text("수치 미입력은 미정으로 남습니다. 티켓 완료 건수와 KPI를 합산하지 않습니다.")
                .font(.caption)
                .foregroundStyle(.secondary)
            if let errorMessage {
                Text(errorMessage).font(.caption).foregroundStyle(.red)
            }
            HStack {
                Spacer()
                Button("취소") { dismiss() }
                Button("저장") { Task { await save() } }
                    .buttonStyle(.borderedProminent)
                    .disabled(isSaving || title.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
                        || periodStart.isEmpty)
                    .accessibilityIdentifier("workBoardSaveGoal")
            }
        }
        .padding(22)
        .frame(width: 500)
    }

    @MainActor
    private func save() async {
        guard !isSaving else { return }
        isSaving = true
        defer { isSaving = false }
        do {
            try await client.saveGoal(
                WorkBoardGoalInput(
                    projectId: draft.goal == nil ? draft.projectId : nil,
                    horizon: horizon,
                    periodStart: periodStart,
                    title: title,
                    description: description,
                    metricName: metricName.isEmpty ? nil : metricName,
                    metricUnit: metricUnit.isEmpty ? nil : metricUnit,
                    targetValue: targetValue.isEmpty ? nil : targetValue,
                    actualValue: actualValue.isEmpty ? nil : actualValue
                ),
                goalId: draft.goal?.id
            )
            dismiss()
            onSaved()
        } catch {
            errorMessage = error.localizedDescription
        }
    }
}

private struct WorkBoardTicketEditor: View {
    let client: WorkBoardClient
    let draft: WorkBoardDraft
    let projectKey: String
    let assignees: [WorkBoardAssignee]
    let tickets: [WorkBoardTicket]
    let goals: [WorkBoardGoal]
    let workRecords: [WorkBoardRecord]
    let onSaved: () -> Void

    @Environment(\.dismiss) private var dismiss
    @State private var title: String
    @State private var description: String
    @State private var assigneeId: String
    @State private var completedById: String
    @State private var verifiedById: String
    @State private var state: WorkBoardState
    @State private var pushedCommitSha: String
    @State private var dueDate: String
    @State private var completionCriteria: String
    @State private var decisionPending: Bool
    @State private var parentTicketId: String
    @State private var predecessorIds: Set<String>
    @State private var goalIds: Set<String>
    @State private var workRecordIds: Set<String>
    @State private var recordQuery = ""
    @State private var recordResults: [WorkBoardRecord] = []
    @State private var recordDetail: WorkBoardRecord?
    @State private var activity: [WorkBoardActivity] = []
    @State private var activityKind = "progress"
    @State private var activityBody = ""
    @State private var isAddingActivity = false
    @State private var isSearching = false
    @State private var isSaving = false
    @State private var errorMessage: String?

    init(
        client: WorkBoardClient,
        draft: WorkBoardDraft,
        projectKey: String,
        assignees: [WorkBoardAssignee],
        tickets: [WorkBoardTicket],
        goals: [WorkBoardGoal],
        workRecords: [WorkBoardRecord],
        onSaved: @escaping () -> Void
    ) {
        self.client = client
        self.draft = draft
        self.projectKey = projectKey
        self.assignees = assignees
        self.tickets = tickets
        self.goals = goals
        self.workRecords = workRecords
        self.onSaved = onSaved
        _title = State(initialValue: draft.ticket?.title ?? "")
        _description = State(initialValue: draft.ticket?.description ?? "")
        _assigneeId = State(initialValue: draft.ticket?.assigneeId ?? "")
        _completedById = State(initialValue: draft.ticket?.completedById ?? "")
        _verifiedById = State(initialValue: draft.ticket?.verifiedById ?? "")
        _state = State(initialValue: WorkBoardState(rawValue: draft.ticket?.state ?? "open") ?? .open)
        _pushedCommitSha = State(initialValue: draft.ticket?.pushedCommitSha ?? "")
        _dueDate = State(initialValue: draft.ticket?.dueDate ?? "")
        _completionCriteria = State(initialValue: draft.ticket?.completionCriteria ?? "")
        _decisionPending = State(initialValue: draft.ticket?.decisionPending ?? false)
        _parentTicketId = State(initialValue: draft.ticket?.parentTicketId ?? "")
        _predecessorIds = State(initialValue: Set(draft.ticket?.predecessorIds ?? []))
        _goalIds = State(initialValue: Set(draft.ticket?.goalIds ?? []))
        _workRecordIds = State(initialValue: Set(draft.ticket?.workRecordIds ?? []))
    }

    var body: some View {
        VStack(spacing: 0) {
            HStack {
                Label(draft.ticket == nil ? "티켓 만들기" : "티켓 수정", systemImage: "checklist")
                    .font(.system(size: 13, weight: .bold))
                    .foregroundStyle(DashboardPalette.accent)
                Spacer()
                Button("닫기") { dismiss() }
                    .buttonStyle(.bordered)
                    .keyboardShortcut(.cancelAction)
            }
            .padding(.horizontal, 20)
            .frame(height: 54)
            Divider().opacity(0.5)
            ScrollView {
                VStack(alignment: .leading, spacing: 14) {
                    editorSection("기본 정보", systemImage: "text.alignleft") {
                        TextField("제목", text: $title)
                            .font(.system(size: 15, weight: .semibold))
                        TextField("설명", text: $description, axis: .vertical)
                            .lineLimit(3...6)
                    }
                    HStack(alignment: .top, spacing: 14) {
                        VStack(spacing: 14) {
                            editorSection("상태와 담당", systemImage: "person.crop.circle") {
                                Picker("상태", selection: $state) {
                                    ForEach(WorkBoardState.allCases) { item in
                                        Text(item.label).tag(item)
                                    }
                                }
                                Picker("담당자", selection: $assigneeId) {
                                    Text("미정").tag("")
                                    ForEach(assignees) { person in
                                        Text(person.name).tag(person.id)
                                    }
                                }
                                if state == .review || state == .done {
                                    Picker("완료자", selection: $completedById) {
                                        Text("미확인").tag("")
                                        ForEach(assignees) { person in
                                            Text(person.name).tag(person.id)
                                        }
                                    }
                                    Picker("검증자", selection: $verifiedById) {
                                        Text("미확인").tag("")
                                        ForEach(assignees) { person in
                                            Text(person.name).tag(person.id)
                                        }
                                    }
                                    Text("완료자·검증자는 기록에 근거해 지정합니다.")
                                        .font(.caption).foregroundStyle(.secondary)
                                }
                            }
                            editorSection("완료 기준", systemImage: "checkmark.seal") {
                                TextField("완료 조건", text: $completionCriteria, axis: .vertical)
                                    .lineLimit(3...6)
                                if projectKey != "toss-trading" {
                                    Text("오피스 티켓은 검토 후 커밋·푸시까지 마쳐야 완료(반영 완료)입니다.")
                                        .font(.caption).foregroundStyle(.secondary)
                                    TextField("원격에 푸시된 40자리 커밋 SHA", text: $pushedCommitSha)
                                        .font(.system(.caption, design: .monospaced))
                                }
                                TextField("기한 미정 · 입력 시 YYYY-MM-DD", text: $dueDate)
                                Toggle("사용자 결정 대기", isOn: $decisionPending)
                                    .help("티켓 상태와 별도로 표시합니다")
                            }
                        }
                        .frame(maxWidth: .infinity, alignment: .topLeading)
                        editorSection("작업 연결", systemImage: "link") {
                            Picker("WBS 상위 작업", selection: $parentTicketId) {
                                Text("없음").tag("")
                                ForEach(tickets.filter { $0.id != draft.ticket?.id }) { ticket in
                                    Text(ticket.title).tag(ticket.id)
                                }
                            }
                            DisclosureGroup("선행 작업 · \(predecessorIds.count)건") {
                                if tickets.filter({ $0.id != draft.ticket?.id }).isEmpty {
                                    Text("연결할 티켓 없음").font(.caption).foregroundStyle(.secondary)
                                }
                                ForEach(tickets.filter { $0.id != draft.ticket?.id }) { ticket in
                                    Toggle(ticket.title, isOn: Binding(
                                        get: { predecessorIds.contains(ticket.id) },
                                        set: { enabled in
                                            if enabled { predecessorIds.insert(ticket.id) }
                                            else { predecessorIds.remove(ticket.id) }
                                        }
                                    ))
                                    .font(.caption)
                                }
                            }
                            DisclosureGroup("연결 목표 · \(goalIds.count)건") {
                                if goals.isEmpty {
                                    Text("생성된 목표 없음").font(.caption).foregroundStyle(.secondary)
                                }
                                ForEach(goals) { goal in
                                    Toggle("\(goal.title) · \(goal.periodStart)", isOn: Binding(
                                        get: { goalIds.contains(goal.id) },
                                        set: { enabled in
                                            if enabled { goalIds.insert(goal.id) }
                                            else { goalIds.remove(goal.id) }
                                        }
                                    ))
                                    .font(.caption)
                                }
                            }
                        }
                        .frame(maxWidth: .infinity, alignment: .topLeading)
                    }
                    editorSection("업무·대화 근거", systemImage: "doc.text") {
                        Text("연결 기록은 참고 자료이며 직원의 현재 실행 상태를 증명하지 않습니다.")
                            .font(.caption).foregroundStyle(.secondary)
                        ForEach(workRecords.filter { workRecordIds.contains($0.id) }) { record in
                            HStack(alignment: .top) {
                                VStack(alignment: .leading, spacing: 3) {
                                    Text("\(record.title) · \(record.recordedAt.prefix(10)) · \(assignees.first { $0.id == record.characterId }?.name ?? "작성자 미확인")")
                                        .font(.caption.bold())
                                    Text(record.resultPreview)
                                        .font(.caption).foregroundStyle(.secondary).lineLimit(2)
                                }
                                Spacer()
                                Button("결과 보기") { Task { await showRecord(record.id) } }
                                Button("해제") { workRecordIds.remove(record.id) }
                            }
                        }
                        HStack {
                            TextField("기록 검색어", text: $recordQuery)
                                .textFieldStyle(.roundedBorder)
                            Button("검색") { Task { await searchRecords() } }
                                .disabled(isSearching || recordQuery.trimmingCharacters(in: .whitespacesAndNewlines).count < 2)
                        }
                        ForEach(recordResults) { record in
                            HStack {
                                Toggle("\(record.title) · \(assignees.first { $0.id == record.characterId }?.name ?? "작성자 미확인")", isOn: Binding(
                                    get: { workRecordIds.contains(record.id) },
                                    set: { enabled in
                                        if enabled { workRecordIds.insert(record.id) }
                                        else { workRecordIds.remove(record.id) }
                                    }
                                ))
                                .font(.caption)
                                Button("내용") { recordDetail = record }
                            }
                        }
                    }
                    if let ticketId = draft.ticket?.id {
                        editorSection("진행 기록 · 최근 100건", systemImage: "clock.arrow.circlepath") {
                            Text("작성자는 요청자가 신고한 값이며 신원 확인을 뜻하지 않습니다.")
                                .font(.caption).foregroundStyle(.secondary)
                            ForEach(activity) { item in
                                VStack(alignment: .leading, spacing: 2) {
                                    Text("\(item.kind) · \(item.createdAt.prefix(16)) · \(item.reportedActorId ?? "작성자 미확인")")
                                        .font(.caption).foregroundStyle(.secondary)
                                    Text(item.summary).font(.system(size: 12)).textSelection(.enabled)
                                }
                            }
                            HStack {
                                Picker("기록 종류", selection: $activityKind) {
                                    Text("진행").tag("progress")
                                    Text("결정").tag("decision")
                                    Text("검증").tag("verification")
                                }
                                TextField("진행 내용", text: $activityBody, axis: .vertical)
                                    .lineLimit(2...4)
                                Button("기록 추가") { Task { await addActivity(ticketId) } }
                                    .disabled(isAddingActivity || activityBody.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                                    .accessibilityIdentifier("workBoardAddActivity")
                            }
                        }
                    }
                }
                .padding(18)
            }
            Divider().opacity(0.5)
            HStack {
                Text(errorMessage ?? "담당자 지정은 실행 지시가 아닙니다.")
                    .font(.caption)
                    .foregroundStyle(errorMessage == nil ? Color.secondary : Color.red)
                Spacer()
                Button("취소") { dismiss() }
                    .buttonStyle(.bordered)
                Button("저장") { Task { await save() } }
                    .buttonStyle(.borderedProminent)
                    .tint(DashboardPalette.accent)
                    .disabled(isSaving || title.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
                    .accessibilityIdentifier("workBoardSaveTicket")
            }
            .padding(.horizontal, 20)
            .frame(height: 58)
        }
        .frame(width: 900, height: 720)
        .background(
            LinearGradient(
                colors: [DashboardPalette.accent.opacity(0.055), Color.primary.opacity(0.012)],
                startPoint: .topLeading,
                endPoint: .bottomTrailing
            )
        )
        .task {
            if let ticketId = draft.ticket?.id { await loadActivity(ticketId) }
        }
        .sheet(item: $recordDetail) { record in
            VStack(spacing: 0) {
                HStack {
                    Text("업무 기록").font(.headline)
                    Spacer()
                    Button("닫기") { recordDetail = nil }
                        .keyboardShortcut(.cancelAction)
                        .accessibilityIdentifier("workBoardCloseRecord")
                }
                .padding(14)
                Divider()
                ScrollView {
                    VStack(alignment: .leading, spacing: 10) {
                        Text(record.title).font(.headline)
                        Text("\(record.recordType) · \(assignees.first { $0.id == record.characterId }?.name ?? "작성자 미확인") · \(record.recordedAt)")
                            .font(.caption).foregroundStyle(.secondary)
                        if let sourceTurnId = record.sourceTurnId {
                            Text("관련 대화 ID: \(sourceTurnId)")
                                .font(.caption).textSelection(.enabled)
                        }
                        Text(record.body ?? "본문 없음")
                            .font(.body).textSelection(.enabled)
                    }
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .padding(20)
                }
            }
            .frame(width: 460, height: 440)
        }
    }

    private func editorSection<Content: View>(
        _ title: String,
        systemImage: String,
        @ViewBuilder content: () -> Content
    ) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            Label(title, systemImage: systemImage)
                .font(.system(size: 12, weight: .bold))
                .foregroundStyle(DashboardPalette.accent)
            content()
        }
        .workBoardSurface()
    }

    @MainActor
    private func loadActivity(_ ticketId: String) async {
        do {
            activity = try await client.loadActivity(ticketId)
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    @MainActor
    private func addActivity(_ ticketId: String) async {
        guard !isAddingActivity else { return }
        isAddingActivity = true
        defer { isAddingActivity = false }
        do {
            try await client.addActivity(
                WorkBoardActivityInput(kind: activityKind, body: activityBody),
                ticketId: ticketId
            )
            activityBody = ""
            await loadActivity(ticketId)
            errorMessage = nil
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    @MainActor
    private func searchRecords() async {
        guard !isSearching else { return }
        isSearching = true
        defer { isSearching = false }
        do {
            recordResults = try await client.searchRecords(recordQuery.trimmingCharacters(in: .whitespacesAndNewlines))
            errorMessage = nil
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    @MainActor
    private func showRecord(_ id: String) async {
        do {
            recordDetail = try await client.loadRecord(id)
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    @MainActor
    private func save() async {
        guard !isSaving else { return }
        isSaving = true
        defer { isSaving = false }
        do {
            try await client.save(
                WorkBoardTicketInput(
                    projectId: draft.ticket == nil ? draft.projectId : nil,
                    title: title,
                    description: description,
                    assigneeId: assigneeId.isEmpty ? nil : assigneeId,
                    completedById: (state == .review || state == .done) && !completedById.isEmpty ? completedById : nil,
                    verifiedById: (state == .review || state == .done) && !verifiedById.isEmpty ? verifiedById : nil,
                    state: state.rawValue,
                    pushedCommitSha: pushedCommitSha.isEmpty ? nil : pushedCommitSha,
                    dueDate: dueDate.isEmpty ? nil : dueDate,
                    completionCriteria: completionCriteria,
                    decisionPending: decisionPending,
                    parentTicketId: parentTicketId.isEmpty ? nil : parentTicketId,
                    predecessorIds: predecessorIds.sorted(),
                    goalIds: goalIds.sorted(),
                    workRecordIds: workRecordIds.sorted()
                ),
                ticketId: draft.ticket?.id
            )
            dismiss()
            onSaved()
        } catch {
            errorMessage = error.localizedDescription
        }
    }
}
