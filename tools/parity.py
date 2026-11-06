"""Re-runs the parity checks (`ror compare`; Studio captures are cached, so only ror renders again
unless the model or camera changed): Studio's R15 and R6 rigs and a few classic catalog items (gear and SpecialMesh hats, by asset id, iso views), and prints each one's agreement
next to the previous report's.

    python tools/parity.py [name ...]
"""

import json
import os
import subprocess
import sys

ROR = os.path.join("target", "release", "ror.exe" if os.name == "nt" else "ror")
# Studio's rigs, from the content folder ror reads (ROR_ROBLOX_CONTENT: Studio's `content` folder)
STUDIO_AVATAR = os.path.join(os.environ.get("ROR_ROBLOX_CONTENT", "content"), "avatar", "characterR15.rbxm")

# name: (input, eye, look direction, fov); None: the default views (or the views named after "@")
CLOSE_UPS = {
	"r15": (STUDIO_AVATAR, None, None, None),
	"r6": (STUDIO_AVATAR.replace("characterR15.rbxm", "character.rbxm"), None, None, None),
	# the Linked Sword, the Valkyrie Helm, the Dominus Empyreus, the classic red cap
	"catalog-sword": ("125013769@iso", None, None, None),
	"catalog-valkyrie": ("1365767@iso", None, None, None),
	"catalog-dominus": ("21070012@iso", None, None, None),
	"catalog-cap": ("48474313@iso", None, None, None),
}


def pixels(report):
	view = json.load(open(report))["views"][0]["pixels"]
	return view["exactPercent"], view["within8Percent"]


def main():
	names = sys.argv[1:] or list(CLOSE_UPS)
	for name in names:
		source, eye, look, fov = CLOSE_UPS[name]
		source, _, views = source.partition("@")
		out = os.path.join("out", "compare", name)
		report = os.path.join(out, "report.json")
		before = pixels(report) if os.path.exists(report) else None
		command = [ROR, "compare", source, "--out", out]
		if views:
			command += ["--views", views]
		if eye is not None:
			target = [e + 10.0 * d for e, d in zip(eye, look)]
			command += ["--camera", json.dumps({"mode": "lookAt", "eye": eye, "target": target, "fov": fov})]
		result = subprocess.run(command, capture_output=True, text=True)
		if result.returncode != 0:
			print(f"{name}: FAILED {result.stderr.strip()[-300:]}")
			continue
		after = pixels(report)
		previous = f" (before {before[0]:.2f} / {before[1]:.2f})" if before else ""
		print(f"{name:24s} within 2 {after[0]:6.2f}%  within 8 {after[1]:6.2f}%{previous}")


main()
