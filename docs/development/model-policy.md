# Initial model policy

The connector requests only the models Crumb currently exposes. The source
snapshot is `docs/pollinations_models.md`; runtime identifiers are centralized
in `crumb.elixpo/lib/model-policy.ts`.

| Modality | Initial model |
| --- | --- |
| Harness text | `nova-fast`, `qwen-coder` |
| Image | `flux`, `klein` |
| Video | `wan-fast` |
| Audio | `elevenflash` |
| 3D | `trellis-2` |
| Transcription | `whisper` |
| Embeddings | `openai-3-small` |

The harness will combine the two text models behind one replaceable text-model
policy. Every other modality begins with one default except image, where both
Flux and Klein are intentional user-facing choices.
