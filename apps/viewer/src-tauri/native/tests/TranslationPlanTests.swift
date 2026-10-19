import Foundation
import Translation

// Run from apps/viewer/src-tauri (no language downloads or app UI):
// xcrun --sdk macosx swiftc -parse-as-library -swift-version 6 \
//   native/TranslationBridge.swift native/tests/TranslationPlanTests.swift \
//   -o /tmp/tokn-translation-plan-tests && /tmp/tokn-translation-plan-tests
@main
struct TranslationPlanTests {
  static func main() {
    guard #available(macOS 15.0, *) else { return }
    func check(_ condition: @autoclosure () -> Bool, _ message: String) {
      precondition(condition(), message)
    }
    let engine = translationFailure(for: TranslationError.internalError)
    check(engine.recovery == "translation_settings", "Public engine errors should offer Translation settings")
    check(engine.error.contains(TranslationError.internalError.localizedDescription), "Apple's failure description was lost")
    check(engine.error.contains("Check or download"), "Engine failures should suggest checking language downloads")
    if let reason = TranslationError.internalError.failureReason, !reason.isEmpty {
      check(engine.error.contains(reason), "Apple's engine failure reason was lost")
    }
    for error in [TranslationError.unsupportedSourceLanguage, .unsupportedTargetLanguage, .unsupportedLanguagePairing, .unableToIdentifyLanguage, .nothingToTranslate] {
      let failure = translationFailure(for: error)
      check(failure.recovery == nil, "Non-engine Translation errors must not offer settings")
      check(failure.error.contains(error.localizedDescription), "Apple's non-engine failure description was lost")
      if let reason = error.failureReason, !reason.isEmpty {
        check(failure.error.contains(reason), "Apple's non-engine failure reason was lost")
      }
    }
    let unknown = translationFailure(for: NSError(domain: "TranslationTest", code: 42, userInfo: [
      NSLocalizedDescriptionKey: "Translation failed.",
      NSLocalizedFailureReasonErrorKey: "A useful native failure reason."
    ]))
    check(unknown.error.contains("Translation failed."), "Native error description was lost")
    check(unknown.error.contains("A useful native failure reason."), "Native failure reason was lost")
    check(unknown.recovery == nil, "Arbitrary native errors must not offer Translation settings")
    let cancelled = translationFailure(for: CancellationError())
    check(cancelled.error == "Translation cancelled.", "Cancellation should retain its clear message")
    check(cancelled.recovery == nil, "Cancellation must not offer settings")

    let fragmented = ["We can use a local translator to translate this response and keep the original formatting.", "a", "in", "CLI", "API", "Rust"]
    let fragments = translationPlan(for: fragmented)
    check(Set(fragments.batches.map { $0.language }) == ["en"], "Fragments must inherit English context")
    check(fragments.reassemble(fragments.texts) == fragmented, "Fragment reassembly changed input")

    let multilingual = translationPlan(for: ["This response includes an English sentence that explains the translation feature.", "Bonjour, nous allons créer une application pour traduire cette réponse."])
    check(Set(multilingual.batches.map { $0.language }) == ["en", "fr"], "Long foreign prose lost its own language")

    let chinese = translationPlan(for: ["我们将检查 API 请求并修复错误。具体用法请参阅文档。"])
    check(chinese.batches.isEmpty, "Existing Chinese should remain original")

    let mixed = ["这个模块负责翻译回答。具体用法请参阅文档。File not found. Please retry."]
    let mixed_plan = translationPlan(for: mixed)
    let requested = mixed_plan.batches.flatMap { $0.indices }.map { mixed_plan.texts[$0] }
    check(requested == ["File not found.", "Please retry."], "Chinese paragraph hid English sentences: \(requested)")
    check(mixed_plan.reassemble(mixed_plan.texts) == mixed, "Mixed paragraph changed during reassembly")

    let clause = ["请检查路径 File not found and try again 然后继续。"]
    let clause_plan = translationPlan(for: clause)
    let clause_requested = clause_plan.batches.flatMap { $0.indices }.map { clause_plan.texts[$0] }
    check(clause_requested == ["File not found and try again"], "Embedded English clause was skipped: \(clause_requested)")
    check(clause_plan.reassemble(clause_plan.texts) == clause, "Embedded clause reassembly changed input")

    for inputs in [["  First sentence.\r\n\nSecond sentence.  "], ["你好。\n\nFile not found!\t"], ["Hello", "", "  ", "123"], ["A? B! C."]] {
      let plan = translationPlan(for: inputs)
      check(plan.reassemble(plan.texts) == inputs, "Whitespace/punctuation round trip failed")
    }
    let long_input = [String(repeating: "This sentence explains how a local translator preserves the response. ", count: 300)]
    let long_plan = translationPlan(for: long_input)
    check(long_plan.batches.allSatisfy { $0.indices.count <= 128 }, "Native sentence batches must stay bounded")
    check(long_plan.reassemble(long_plan.texts) == long_input, "Long response round trip failed")
    print("TranslationPlan: error recovery, context, multilingual, Chinese, embedded-clause, and round-trip tests passed")
  }
}
