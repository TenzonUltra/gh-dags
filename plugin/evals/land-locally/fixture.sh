source "$(dirname "${BASH_SOURCE[0]}")/../setup.sh"
# main ← theme ← auth, though auth doesn't need theme.
gh dags new theme >/dev/null
echo "dark" > theme.txt
git add theme.txt
git commit -q -m "Add theme"
gh dags new auth --on main >/dev/null
echo "def login(): ..." > auth.py
git add auth.py
git commit -q -m "Add auth"
gh dags move auth --onto theme >/dev/null
