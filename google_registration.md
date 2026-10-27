# Google Cloud registration

How to create the Google OAuth client Calendarchy needs for Calendar, Contacts, and Tasks.
The app reads the credentials from `CALENDARCHY_GOOGLE_CLIENT_ID` and
`CALENDARCHY_GOOGLE_CLIENT_SECRET`; there is no bundled client.

Google's console layout changes from time to time, so menu names may differ slightly.

## One-time project setup

1. **Create a dedicated Google account** for the project (a plain @gmail.com address is
   fine; Google Workspace adds nothing for a public app). Sign in to
   <https://console.cloud.google.com> with it.
2. **Create a project** (e.g. "Calendarchy"). In IAM, add a second Owner so a backup
   account can still reach the project.
3. **Enable three APIs** under APIs & Services → Library:
   - Google Calendar API (events)
   - People API (contacts)
   - Google Tasks API (tasks)

## Consent screen

4. **Open Google Auth Platform → Get started.** Enter the app name, a support email,
   and choose the **External** audience.
5. **Stay in Testing for now.** Under Audience, add the Google accounts you'll sign in
   with as test users (up to 100). In Testing, refresh tokens expire after 7 days.
6. **Add scopes under Data Access:**

   ```
   openid
   https://www.googleapis.com/auth/userinfo.email
   https://www.googleapis.com/auth/calendar.events
   https://www.googleapis.com/auth/calendar.calendarlist.readonly
   https://www.googleapis.com/auth/contacts.readonly
   https://www.googleapis.com/auth/contacts.other.readonly
   https://www.googleapis.com/auth/tasks
   ```

   - Use `contacts.readonly` if the app only displays contacts; use `contacts` (full)
     if users will create or edit them.
   - `contacts.other.readonly` is optional. It covers people the user has emailed but
     never saved, which helps autocomplete attendee addresses.
   - Use `tasks` (read/write) so users can create and complete tasks; use
     `tasks.readonly` if the app only displays them.
   - Add `calendar.calendarlist` (read/write) once calendar color/visibility changes
     or calendar subscriptions ship.

## Client credentials

7. **Create the client** under Clients → Create client. Application type
   **Desktop app**. No redirect URIs are needed; desktop clients accept any loopback
   port, which is what the app binds.
8. **Download the JSON immediately.** Google shows the client secret only at creation.
   If it's lost, create a new secret.
9. **Set the environment variables.** For a terminal launch:

   ```sh
   export CALENDARCHY_GOOGLE_CLIENT_ID="…apps.googleusercontent.com"
   export CALENDARCHY_GOOGLE_CLIENT_SECRET="…"
   ```

   An app started from the Omarchy launcher doesn't read your shell profile. Put the
   same lines in the session environment file instead (on a uwsm-managed Hyprland
   session, `~/.config/uwsm/env`), then log out and back in.

## Before a public release

10. **Fill in Branding:** homepage, privacy policy URL, and an authorized domain
    verified in Search Console. A cheap custom domain is the smoothest path; github.io
    can be awkward to get accepted. The privacy policy must describe how calendar,
    contact, and task data are used.
11. **Publish and submit for verification** with a demo video (uploaded to YouTube)
    showing each requested scope in use. Calendar, Contacts, and Tasks scopes are "sensitive",
    not "restricted", so verification is free and needs no paid security assessment.
    Until verified, users see an "unverified app" warning and sign-ins cap at 100.

**Verify once, after Contacts and Tasks work.** Reviewers reject scopes the app doesn't visibly
use, and adding a scope after verification means another review.

## Cost and quota notes

- The Calendar, People, and Tasks APIs have no per-request charge and need no billing account.
- All users share one project quota, roughly 1M Calendar requests/day by default.
  Polling every 5 minutes per calendar is ~864k requests/day at 1,000 users with
  3 calendars each, so scaling past that needs some of:
  - a free quota-increase request to Google (not guaranteed for large jumps),
  - less frequent polling for calendars not in view, and backoff when the window is
    hidden,
  - rare calendar-list fetches,
  - push notifications (watch channels), which need a public HTTPS endpoint and so
    an ongoing hosting cost.
- The client secret ships inside the app, so anyone can copy it and spend the quota.
  Thunderbird and GNOME Online Accounts accept the same risk. Users can always set the
  environment variables to use their own client.

Policy details here are from memory as of 2026; check Google's current docs before
launch.
