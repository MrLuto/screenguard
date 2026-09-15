"""Run: python -m unittest discover -s webui -p 'test_unifi.py'."""
import unittest
from unittest.mock import patch
import app

class Response:
    ok = True
    def __init__(self, data):
        self.data = data
    def json(self):
        return self.data

class UniFiUiTests(unittest.TestCase):
    def setUp(self):
        app.app.config['TESTING'] = True
        self.client = app.app.test_client()
        with self.client.session_transaction() as session:
            session['token'] = 'test'
            session['username'] = 'parent'

    def test_empty_and_populated_page(self):
        for connection in [{}, {'configured': True, 'statistics': [
            {'name': 'Gateway', 'cpu': 1, 'memory': 20, 'rx_bps': None, 'tx_bps': 40}
        ]}]:
            payload = {'connection': connection, 'clients': [{
                'mac': '02:11:22:33:44:55', 'data': {'name': '<script>', 'type': 'WIRED'},
                'last_seen': 0, 'mode': 'observe', 'excluded': True, 'result': 'preview'
            }]}
            with patch.object(app, 'api', side_effect=[Response(payload), Response({'profiles': []})]):
                response = self.client.get('/unifi')
                self.assertEqual(response.status_code, 200)
                self.assertIn(b'&lt;script&gt;', response.data)

    def test_csrf_blocks_mutation(self):
        with patch.object(app, 'api') as api:
            self.assertEqual(self.client.post('/unifi', data={'action': 'sync'}).status_code, 400)
            api.assert_not_called()

    def test_valid_sync(self):
        with self.client.session_transaction() as session:
            session['unifi_csrf'] = 'test-csrf'
        with patch.object(app, 'api', return_value=Response({})) as api:
            response = self.client.post('/unifi', data={'action': 'sync', 'csrf': 'test-csrf'})
            self.assertEqual(response.status_code, 302)
            api.assert_called_once_with('POST', '/unifi/sync')

    def test_requires_login(self):
        with self.client.session_transaction() as session:
            session.clear()
        with patch.object(app, 'api') as api:
            self.assertEqual(self.client.get('/unifi').status_code, 302)
            api.assert_not_called()
